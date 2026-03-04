use std::sync::Mutex;
use tauri::State;
use crate::server::ServerState;

/// Greet command - sample command for testing
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// Get server port from app state
/// 
/// Reads the port from the ServerState managed by Tauri.
/// This returns the port that the backend server is running on.
#[tauri::command]
pub fn get_server_port(state: State<Mutex<ServerState>>) -> Result<u16, String> {
    let state = state.lock().map_err(|e| e.to_string())?;
    state.port.ok_or_else(|| "Server not ready".to_string())
}

/// Get authentication token
#[tauri::command]
pub async fn get_auth_token() -> Result<Option<String>, String> {
    // TODO: Implement token retrieval from OS keychain
    Ok(None)
}

/// Set authentication token
#[tauri::command]
pub async fn set_auth_token(_token: String) -> Result<(), String> {
    // TODO: Implement token storage in OS keychain
    Ok(())
}

/// Clear authentication token
#[tauri::command]
pub async fn clear_auth_token() -> Result<(), String> {
    // TODO: Implement token removal from OS keychain
    Ok(())
}
