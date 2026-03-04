use tauri::AppHandle;

/// Greet command - sample command for testing
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// Get server port from app state
#[tauri::command]
pub fn get_server_port(_app: AppHandle) -> Result<u16, String> {
    // TODO: Implement server port retrieval from app state
    // This will be wired up once server.rs is implemented
    Ok(8080)
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
