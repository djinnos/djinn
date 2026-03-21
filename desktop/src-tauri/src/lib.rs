use std::sync::Mutex;
use tauri::{Emitter, Manager};
use tokio_util::sync::CancellationToken;

mod auth;
mod commands;
mod connection_mode;
mod server;
mod token_refresh;
mod token_sync;

pub use server::ServerState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Shared cancellation token — cancelled on window close to trigger
    // graceful shutdown of the embedded server.
    let shutdown_token = CancellationToken::new();
    let shutdown_token_for_exit = shutdown_token.clone();
    let shutdown_token_for_setup = shutdown_token.clone();

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
        .manage(shutdown_token)
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let cancel = shutdown_token_for_setup;

            // Start or connect to server based on configured connection mode.
            tauri::async_runtime::spawn(async move {
                let mode = connection_mode::load();

                let startup_result = match mode {
                    connection_mode::ConnectionMode::Embedded => {
                        log::info!("Connection mode: embedded — starting server in-process");
                        server::start_embedded(cancel).await
                    }
                    connection_mode::ConnectionMode::Remote { ref url } => {
                        log::info!("Connection mode: remote — connecting to {url}");
                        if server::check_remote(url).await {
                            Ok(url.clone())
                        } else {
                            Err(format!("Remote server at {url} is not reachable"))
                        }
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
        .on_window_event(move |_window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // Signal the embedded server to shut down gracefully.
                shutdown_token_for_exit.cancel();
            }
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
            commands::setup_git_remote,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
