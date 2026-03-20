use std::sync::Mutex;
use tauri::{Emitter, Manager};

mod auth;
mod commands;
mod server;
mod token_refresh;

pub use server::ServerState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = app.get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(server::init_server_state()))
        .setup(move |app| {
            let app_handle = app.handle().clone();

            // Spawn server in background
            tauri::async_runtime::spawn(async move {
                let startup_result = if let Some(port) = server::discover_server_for_app(&app_handle) {
                    if server::health_check(port).await {
                        log::info!("Discovered healthy existing server on port {}", port);
                        Ok(port)
                    } else {
                        log::warn!(
                            "Discovered existing server on port {} but health check failed; spawning new server",
                            port
                        );
                        server::spawn_server(&app_handle, 30).await
                    }
                } else {
                    server::spawn_server(&app_handle, 30).await
                };

                match startup_result {
                    Ok(port) => {
                        log::info!("Server ready on port {}", port);
                        if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                            if let Ok(mut state) = state.lock() {
                                state.port = Some(port);
                                state.mark_healthy();
                            }
                        }

                        // Start background health monitor for server reconnection
                        server::start_health_monitor(&app_handle);

                        // Show the main window now that server is ready
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to spawn server: {}", e);
                        if let Ok(mut state) = app_handle.state::<Mutex<ServerState>>().lock() {
                            state.mark_error(&e);
                        }

                        // Show the main window so the frontend can render error/retry UI
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                        }
                    }
                }
            });

            // Attempt silent authentication on startup
            let silent_auth_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match token_refresh::attempt_silent_auth_on_startup().await {
                    token_refresh::RefreshResult::Success(_state) => {
                        log::info!("Silent authentication successful on startup");
                        // Populate AUTH_SESSION and emit auth:state-changed event
                        if let Err(e) = commands::populate_session_after_silent_refresh(&silent_auth_app).await {
                            log::error!("Failed to populate session after silent refresh: {}", e);
                            let _ = silent_auth_app.emit("auth:login-required", ());
                        }
                    }
                    token_refresh::RefreshResult::NoToken => {
                        log::info!("No refresh token available, user needs to login");
                        // Emit event to frontend indicating auth required
                        let _ = silent_auth_app.emit("auth:login-required", ());
                    }
                    token_refresh::RefreshResult::Failed(reason) => {
                        log::warn!("Silent authentication failed: {}", reason);
                        // Emit event to frontend indicating auth failure
                        let _ = silent_auth_app.emit("auth:silent-refresh-failed", serde_json::json!({
                            "reason": reason,
                        }));
                    }
                }
            });

            // On macOS, set up window close handler
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
        .on_window_event(|_window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // Window close requested - do nothing to server
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_github_login,
            commands::greet,
            commands::get_server_port,
            commands::get_server_status,
            commands::retry_server_discovery,
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
            commands::setup_git_remote,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
