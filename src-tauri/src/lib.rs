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
        .manage(Mutex::new(server::init_server_state(8080)))
        
        // Setup hook - spawn server sidecar, run health check loop
        .setup(|app| {
            let app_handle = app.handle().clone();
            
            // Spawn server discovery and health check in background
            tauri::async_runtime::spawn(async move {
                // First try to discover an existing server
                let mut port = 8080;
                
                if let Some(discovered_port) = server::discover_server() {
                    log::info!("Discovered existing server on port {}", discovered_port);
                    port = discovered_port;
                    
                    // Update state with discovered port
                    if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                        if let Ok(mut state) = state.lock() {
                            state.port = port;
                        }
                    }
                }
                
                // Run health check loop with exponential backoff
                let max_retries = 10;
                let mut retries = 0;
                let mut interval_ms = 2000u64; // Start with 2 seconds
                
                log::info!("Starting health check loop for port {}", port);
                
                loop {
                    if server::health_check(port).await {
                        log::info!("Server on port {} is healthy", port);
                        
                        // Update state to mark as healthy
                        if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                            if let Ok(mut state) = state.lock() {
                                state.port = port;
                            }
                        }
                        
                        // Show the main window once server is healthy
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                        
                        break;
                    }
                    
                    retries += 1;
                    if retries >= max_retries {
                        log::error!("Health check failed after {} retries", max_retries);
                        break;
                    }
                    
                    log::debug!(
                        "Health check failed, retrying in {}ms (attempt {}/{ })",
                        interval_ms,
                        retries,
                        max_retries
                    );
                    
                    tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)).await;
                    
                    // Exponential backoff: double the interval, cap at 30 seconds
                    interval_ms = (interval_ms * 2).min(30000);
                }
            });
            
            Ok(())
        })
        
        // Tauri command handlers
        .invoke_handler(tauri::generate_handler![
            commands::greet,
            commands::get_server_port,
            commands::get_auth_token,
            commands::set_auth_token,
            commands::clear_auth_token,
        ])
        
        // Run the application
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
