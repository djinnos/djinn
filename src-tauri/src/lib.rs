// DjinnOS Desktop - Tauri Application
// 
// Architecture: All Rust logic lives here in lib.rs
// main.rs is a thin shim that calls run()

use std::sync::Mutex;

mod auth;
mod commands;
mod server;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Plugin registration
        .plugin(tauri_plugin_opener::init())
        
        // Managed state
        .manage(Mutex::new(server::init_server_state(8080)))
        
        // Setup hook - spawn server sidecar, etc.
        .setup(|app| {
            // TODO: Spawn server sidecar and start health check loop
            // Server should start with window hidden, then show after health check passes
            
            let _app_handle = app.handle().clone();
            
            // Spawn server in background
            // tauri::async_runtime::spawn(async move {
            //     if let Err(e) = server::spawn_server(&app_handle, 8080).await {
            //         eprintln!("Failed to spawn server: {}", e);
            //     }
            // });
            
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
