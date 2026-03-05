use crate::auth::{build_authorize_url, clear_token, generate_pkce, retrieve_token, store_token, PkceParams};
use crate::server::ServerState;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use once_cell::sync::Lazy;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, serde::Serialize)]
pub struct UserProfile {
    pub sub: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub picture: Option<String>,
}

#[derive(Debug, Clone)]
struct AuthSession {
    access_token: String,
    expires_in: Option<u64>,
    user_profile: Option<UserProfile>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    sub: String,
    name: Option<String>,
    email: Option<String>,
    picture: Option<String>,
}

use serde::Deserialize;
use std::sync::Mutex as StdMutex;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

/// Global storage for PKCE params during OAuth flow
static PKCE_PARAMS: Lazy<StdMutex<Option<PkceParams>>> = Lazy::new(|| StdMutex::new(None));
static AUTH_SESSION: Lazy<StdMutex<Option<AuthSession>>> = Lazy::new(|| StdMutex::new(None));

/// Greet command - sample command for testing
#[tauri::command]
pub fn greet(name: &str) -> String {
    format!("Hello, {}! You have been greeted from Rust!", name)
}

/// Get server port from app state
#[tauri::command]
pub fn get_server_port(state: State<Mutex<ServerState>>) -> Result<u16, String> {
    let state = state
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    state.port.ok_or_else(|| "Server not ready".to_string())
}

/// Server status response
#[derive(serde::Serialize)]
pub struct ServerStatus {
    pub port: Option<u16>,
    pub is_healthy: bool,
    pub has_error: bool,
    pub error_message: Option<String>,
}

/// Get server status from app state
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
    let session = AUTH_SESSION
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    Ok(session.as_ref().map(|s| s.access_token.clone()))
}

/// Set authentication token
#[tauri::command]
pub async fn set_auth_token(token: String) -> Result<(), String> {
    let mut session = AUTH_SESSION
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    *session = Some(AuthSession {
        access_token: token,
        expires_in: None,
        user_profile: None,
    });
    Ok(())
}

/// Clear authentication token
#[tauri::command]
pub async fn clear_auth_token() -> Result<(), String> {
    let mut session = AUTH_SESSION
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    *session = None;
    drop(session);
    clear_token().await?;
    Ok(())
}


#[tauri::command]
pub async fn get_refresh_token() -> Result<Option<String>, String> {
    retrieve_token().await
}

#[tauri::command]
pub async fn set_refresh_token(token: String) -> Result<(), String> {
    store_token(&token).await
}

#[tauri::command]
pub async fn clear_refresh_token() -> Result<(), String> {
    clear_token().await
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



/// Exchange authorization code for tokens at Clerk token endpoint
#[tauri::command]
pub async fn exchange_auth_code(
    code: String,
    code_verifier: String,
    redirect_uri: String,
    client_id: String,
) -> Result<UserProfile, String> {
    let client = reqwest::Client::new();
    let token_url = format!("https://{}/oauth/token", crate::auth::CLERK_DOMAIN);

    let resp = client
        .post(&token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("code_verifier", code_verifier.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Token request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(format!("Token endpoint returned {}: {}", status, body));
    }

    let token_response: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse token response: {}", e))?;

    let user_profile = token_response
        .id_token
        .as_deref()
        .map(decode_id_token)
        .transpose()?;

    let mut auth_session = AUTH_SESSION
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
    *auth_session = Some(AuthSession {
        access_token: token_response.access_token,
        expires_in: token_response.expires_in,
        user_profile: user_profile.clone(),
    });
    drop(auth_session);

    if let Some(refresh_token) = token_response.refresh_token {
        store_token(&refresh_token).await?;
        log::info!("Stored refresh token in secure storage");
    }

    user_profile.ok_or_else(|| "Missing id_token in token response".to_string())
}

fn decode_id_token(id_token: &str) -> Result<UserProfile, String> {
    let mut parts = id_token.split('.');
    let _header = parts.next().ok_or("Invalid id_token format")?;
    let payload = parts.next().ok_or("Invalid id_token format")?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|e| format!("Failed to decode id_token payload: {}", e))?;

    let claims: IdTokenClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|e| format!("Failed to parse id_token claims: {}", e))?;

    Ok(UserProfile {
        sub: claims.sub,
        name: claims.name,
        email: claims.email,
        picture: claims.picture,
    })
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
