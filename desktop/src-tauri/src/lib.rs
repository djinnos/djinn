use std::sync::Mutex;
use tauri::{Emitter, Manager};

mod auth;
mod commands;
mod connection_mode;
mod deploy;
mod server;
mod ssh_hosts;
mod ssh_tunnel;
mod token_refresh;
mod token_sync;
mod wsl;

pub use server::ServerState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = app
                .get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(server::init_server_state()))
        .setup(move |app| {
            let app_handle = app.handle().clone();

            // Start or connect to server based on configured connection mode.
            tauri::async_runtime::spawn(async move {
                let mode = connection_mode::load();

                let startup_result = match mode.clone() {
                    connection_mode::ConnectionMode::Daemon => {
                        log::info!("Connection mode: daemon — ensuring server is running");
                        server::ensure_daemon().await
                    }
                    connection_mode::ConnectionMode::Remote { ref url } => {
                        log::info!("Connection mode: remote — connecting to {url}");
                        if server::check_remote(url).await {
                            Ok(url.clone())
                        } else {
                            Err(format!("Remote server at {url} is not reachable"))
                        }
                    }
                    connection_mode::ConnectionMode::Ssh { ref host_id } => {
                        log::info!("Connection mode: ssh — tunnelling via host {host_id}");
                        let host = ssh_hosts::find_host(host_id);
                        match host {
                            Some(host) => {
                                if let Err(e) = ssh_tunnel::ensure_remote_daemon(&host).await {
                                    log::warn!("Could not ensure remote daemon: {e}");
                                }
                                match ssh_tunnel::start_tunnel(&host) {
                                    Ok(tunnel) => {
                                        let base_url = format!("http://127.0.0.1:{}", tunnel.local_port);
                                        let local_port = tunnel.local_port;
                                        ssh_tunnel::set_active_tunnel(tunnel);

                                        // Wait for health through tunnel.
                                        let mut ok = false;
                                        for _ in 0..40 {
                                            if server::health_check(&base_url).await {
                                                ok = true;
                                                break;
                                            }
                                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                                        }
                                        if ok {
                                            if let Some(state) = app_handle.try_state::<std::sync::Mutex<ServerState>>() {
                                                if let Ok(mut s) = state.lock() {
                                                    s.tunnel_status = ssh_tunnel::TunnelStatus::Connected { local_port };
                                                }
                                            }
                                            Ok(base_url)
                                        } else {
                                            Err("SSH tunnel established but daemon not reachable".into())
                                        }
                                    }
                                    Err(e) => Err(format!("Failed to start SSH tunnel: {e}")),
                                }
                            }
                            None => Err(format!("SSH host '{host_id}' not found")),
                        }
                    }
                    connection_mode::ConnectionMode::Wsl => {
                        log::info!("Connection mode: WSL");
                        wsl::ensure_wsl_daemon(8372).await
                    }
                };

                match startup_result {
                    Ok(base_url) => {
                        log::info!("Server ready at {base_url}");
                        if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                            if let Ok(mut s) = state.lock() {
                                s.mark_healthy(&base_url);
                            }
                        }

                        server::start_health_monitor(&app_handle);

                        // Start tunnel monitor when in SSH mode.
                        if matches!(mode, connection_mode::ConnectionMode::Ssh { .. }) {
                            server::start_tunnel_monitor(&app_handle);
                        }

                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to connect to server: {e}");
                        if let Ok(mut state) =
                            app_handle.state::<Mutex<ServerState>>().lock()
                        {
                            state.mark_error(&e);
                        }

                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                        }
                    }
                }
            });

            // Attempt silent authentication on startup.
            let silent_auth_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match token_refresh::attempt_silent_auth_on_startup().await {
                    token_refresh::RefreshResult::Success(_state) => {
                        log::info!("Silent authentication successful on startup");
                        if let Err(e) =
                            commands::populate_session_after_silent_refresh(&silent_auth_app)
                                .await
                        {
                            log::error!("Failed to populate session after silent refresh: {e}");
                            let _ = silent_auth_app.emit("auth:login-required", ());
                        }
                    }
                    token_refresh::RefreshResult::NoToken => {
                        log::info!("No refresh token available, user needs to login");
                        let _ = silent_auth_app.emit("auth:login-required", ());
                    }
                    token_refresh::RefreshResult::Failed(reason) => {
                        log::warn!("Silent authentication failed: {reason}");
                        let _ = silent_auth_app.emit(
                            "auth:silent-refresh-failed",
                            serde_json::json!({ "reason": reason }),
                        );
                    }
                }
            });

            // On macOS, prevent window close — exit the whole app instead.
            #[cfg(target_os = "macos")]
            {
                let window = app.get_webview_window("main").expect("no main window");
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        app_handle.exit(0);
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_github_login,
            commands::greet,
            commands::get_server_port,
            commands::get_server_url,
            commands::get_server_status,
            commands::retry_server_connection,
            commands::get_connection_mode,
            commands::set_connection_mode,
            commands::get_auth_token,
            commands::set_auth_token,
            commands::clear_auth_token,
            commands::get_refresh_token,
            commands::set_refresh_token,
            commands::clear_refresh_token,
            commands::auth_get_state,
            commands::auth_login,
            commands::auth_logout,
            commands::perform_token_refresh,
            commands::get_auth_state,
            commands::is_token_expired,
            commands::logout,
            commands::select_directory,
            commands::check_git_remote,
            commands::list_git_branches,
            commands::setup_git_remote,
            commands::sync_github_tokens,
            commands::get_ssh_hosts,
            commands::save_ssh_host,
            commands::remove_ssh_host,
            commands::test_ssh_connection,
            commands::get_tunnel_status,
            commands::deploy_server_to_host,
            commands::check_wsl_available,
        ])
        .build(tauri::generate_context!())
        .expect("error building tauri application")
        .run(|_app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                ssh_tunnel::stop_active_tunnel();
            }
        });
}
