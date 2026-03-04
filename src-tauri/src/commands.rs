use crate::auth::{build_authorize_url, generate_pkce, PkceParams};
use crate::server::ServerState;
use once_cell::sync::Lazy;
use std::sync::Mutex;
use std::sync::Mutex as StdMutex;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
/// Global storage for PKCE params during OAuth flow
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
    let state = state
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    state.port.ok_or_else(|| "Server not ready".to_string())
}

/// Get server status from app state
#[derive(serde::Serialize)]
pub struct ServerStatus {
    pub port: Option<u16>,
    pub is_healthy: bool,
    pub has_error: bool,
    pub error_message: Option<String>,
}

#[tauri::command]
pub fn get_server_status(state: State<Mutex<ServerState>>) -> Result<ServerStatus, String> {
    let state = state
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;

    Ok(ServerStatus {
        port: state.port,
        is_healthy: state.is_healthy,
        has_error: state.has_error,
        error_message: state.error_message.clone(),
    })
}

/// Retry server discovery/spawn
#[tauri::command]
pub async fn retry_server_discovery(app: AppHandle) -> Result<u16, String> {
    crate::server::retry_server_discovery(&app).await
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
    let mut stored = PKCE_PARAMS
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
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
    let stored = PKCE_PARAMS
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    Ok(stored.as_ref().map(|p| p.code_verifier.clone()))
}

/// Clear stored PKCE params after successful authentication
#[tauri::command]
pub fn clear_pkce_params() -> Result<(), String> {
    let mut stored = PKCE_PARAMS
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    *stored = None;
    Ok(())
}
