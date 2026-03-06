use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;

mod auth;
mod auth_callback;
mod commands;
mod dev_server;
mod server;
mod token_refresh;

pub use auth_callback::AuthCallbackManager;
pub use server::ServerState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let auth_callback_manager = Arc::new(AuthCallbackManager::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = app.get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(server::init_server_state()))
        .manage(auth_callback_manager.clone())
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let auth_manager = auth_callback_manager.clone();

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

            // Register deep link handler for djinn:// protocol
            let deep_link_app = app.handle().clone();
            let deep_link_manager = auth_manager.clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    let url_str = url.as_str();
                    log::info!("Received deep link: {}", url_str);
                    
                    if url_str.starts_with("djinn://auth/callback") {
                        match deep_link_manager.handle_callback_url(url_str) {
                            Ok(data) => {
                                if let Err(e) = deep_link_manager.process_callback(&deep_link_app, data) {
                                    log::error!("Failed to process deep link callback: {}", e);
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to handle deep link: {}", e);
                            }
                        }

                        // Show the main window so the frontend can render error/retry UI
                        if let Some(window) = deep_link_app.get_webview_window("main") {
                            let _ = window.show();
                        }
                    }
                }
            });

            // Spawn dev-mode callback server (only in dev builds)
            #[cfg(debug_assertions)]
            {
                let dev_app = app.handle().clone();
                let dev_manager = auth_manager.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = dev_server::spawn_dev_server(dev_app, dev_manager).await {
                        log::error!("Failed to spawn dev callback server: {}", e);
                    }
                });
            }

            // Attempt silent authentication on startup
            let silent_auth_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                match token_refresh::attempt_silent_auth_on_startup().await {
                    token_refresh::RefreshResult::Success(state) => {
                        log::info!("Silent authentication successful on startup");
                        // Emit event to frontend indicating successful silent auth
                        let _ = silent_auth_app.emit("auth:silent-refresh-success", serde_json::json!({
                            "user_id": state.user_id,
                        }));
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
            match event {
                tauri::WindowEvent::CloseRequested { .. } => {
                    // Window close requested - do nothing to server
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
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
            commands::exchange_auth_code,
            commands::initiate_oauth_login,
            commands::get_pkce_code_verifier,
            commands::clear_pkce_params,
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
