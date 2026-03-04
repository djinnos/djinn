use tauri::Manager;
use std::sync::Mutex;

mod auth;
mod commands;
mod server;

/// Server state managed by Tauri
pub use server::ServerState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Plugin registration
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = app.get_webview_window("main")
                .expect("no main window")
                .set_focus();
        }))

        // Managed state
        .manage(Mutex::new(server::init_server_state()))

        // Setup hook - spawn server sidecar
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Spawn server in background
            tauri::async_runtime::spawn(async move {
                match server::spawn_server(&app_handle, 30).await {
                    Ok(port) => {
                        log::info!("Server started successfully on port {}", port);
                    }
                    Err(e) => {
                        log::error!("Failed to spawn server: {}", e);
                        // Update state with error
                        if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                            if let Ok(mut state) = state.lock() {
                                state.mark_error(&e);
                            }
                        }
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

        // Run event handler - do NOT kill server on exit
        .on_window_event(|_window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { .. } => {
                    // Window close requested - do nothing to server
                    // Server continues running as independent daemon
                }
                _ => {}
            }
        })

        // Tauri command handlers
        .invoke_handler(tauri::generate_handler![
            commands::greet,
            commands::get_server_port,
            commands::get_server_status,
            commands::retry_server_discovery,
            commands::get_auth_token,
            commands::set_auth_token,
            commands::clear_auth_token,
        ])

        // Run the application
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
