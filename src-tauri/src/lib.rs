// DjinnOS Desktop - Tauri Application
// 
// Architecture: All Rust logic lives here in lib.rs
// main.rs is a thin shim that calls run()

use std::sync::Mutex;
use tauri::Manager;

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
                        
                        // Show the main window now that server is ready
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                        }
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
            
            Ok(())
        })
        
        // Run event handler - do NOT kill server on exit
        .on_window_event(|_window, event| {
            match event {
                tauri::WindowEvent::CloseRequested { .. } => {
                    // Window close requested - do nothing to server
                }
                _ => {}
            }
        })
        
        // Tauri command handlers
        .invoke_handler(tauri::generate_handler![
            commands::greet,
            commands::get_server_port,
            commands::get_auth_token,
            commands::set_auth_token,
            commands::clear_auth_token,
            commands::initiate_oauth_login,
            commands::get_pkce_code_verifier,
            commands::clear_pkce_params,
        ])
        
        // Run the application
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
