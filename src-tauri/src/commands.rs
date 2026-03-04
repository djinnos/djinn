use std::sync::Mutex;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use crate::auth::{generate_pkce, build_authorize_url, PkceParams};
use crate::server::ServerState;
use std::sync::Mutex as StdMutex;

/// Global storage for PKCE params during OAuth flow
use once_cell::sync::Lazy;
static PKCE_PARAMS: Lazy<StdMutex<Option<PkceParams>>> = Lazy::new(|| StdMutex::new(None));

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
    let state = state.lock().map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    state.port.ok_or_else(|| "Server not ready".to_string())
}

/// Get authentication token
#[tauri::command]
pub async fn get_auth_token() -> Result<Option<String>, String> {
    Ok(None)
}

/// Set authentication token
#[tauri::command]
pub async fn set_auth_token(_token: String) -> Result<(), String> {
    Ok(())
}

/// Clear authentication token
#[tauri::command]
pub async fn clear_auth_token() -> Result<(), String> {
    Ok(())
}

/// Initiate OAuth login with Clerk
/// 
/// Generates PKCE parameters, stores them for later verification,
/// and opens the system browser to the Clerk authorization URL.
#[tauri::command]
pub async fn initiate_oauth_login(app: tauri::AppHandle) -> Result<(), String> {
    // Generate PKCE parameters
    let pkce = generate_pkce();
    
    // Store PKCE params for later verification during callback
    let mut stored = PKCE_PARAMS.lock().map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    *stored = Some(pkce.clone());
    drop(stored);
    
    // Build authorization URL
    let auth_url = build_authorize_url(&pkce);
    
    // Open system browser
    app.opener()
        .open_url(&auth_url, None::<&str>)
        .map_err(|e| format!("Failed to open browser: {}", e))?;
    
    Ok(())
}

/// Get stored PKCE code_verifier (for token exchange)
/// 
/// This should be called during the OAuth callback to retrieve
/// the code_verifier for exchanging the authorization code.
#[tauri::command]
pub fn get_pkce_code_verifier() -> Result<Option<String>, String> {
    let stored = PKCE_PARAMS.lock().map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    Ok(stored.as_ref().map(|p| p.code_verifier.clone()))
}

/// Clear stored PKCE params after successful authentication
#[tauri::command]
pub fn clear_pkce_params() -> Result<(), String> {
    let mut stored = PKCE_PARAMS.lock().map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    *stored = None;
    Ok(())
}
