use crate::auth::{build_authorize_url, clear_token, generate_pkce, retrieve_token, store_token, PkceParams};
use crate::auth_callback::AuthCallbackManager;
use crate::server::ServerState;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use once_cell::sync::Lazy;
use std::sync::Mutex;

// Token refresh module imports
use crate::token_refresh::{self, RefreshResult, TokenState};

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
    user_profile: Option<UserProfile>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    sub: String,
    name: Option<String>,
    email: Option<String>,
    picture: Option<String>,
}

use serde::Deserialize;
use std::sync::{Arc, Mutex as StdMutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

/// Global storage for PKCE params during OAuth flow
static PKCE_PARAMS: Lazy<StdMutex<Option<PkceParams>>> = Lazy::new(|| StdMutex::new(None));
static AUTH_SESSION: Lazy<StdMutex<Option<AuthSession>>> = Lazy::new(|| StdMutex::new(None));

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStateResponse {
    pub is_authenticated: bool,
    pub user: Option<UserProfile>,
}

fn build_auth_state_response(session: Option<&AuthSession>) -> AuthStateResponse {
    AuthStateResponse {
        is_authenticated: session.is_some(),
        user: session.and_then(|s| s.user_profile.clone()),
    }
}

fn emit_auth_state_changed(app: &AppHandle, state: &AuthStateResponse) {
    if let Err(e) = app.emit("auth:state-changed", state) {
        log::warn!("Failed to emit auth:state-changed event: {}", e);
    }
}

/// OAuth configuration returned to the frontend.
/// Single source of truth — the frontend MUST NOT duplicate these values.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthConfig {
    pub client_id: String,
    pub redirect_uri: String,
}

#[tauri::command]
pub fn get_oauth_config() -> OAuthConfig {
    OAuthConfig {
        client_id: crate::auth::CLIENT_ID.to_string(),
        redirect_uri: crate::auth::redirect_uri().to_string(),
    }
}

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

/// Get authentication token (from token refresh state or legacy session)
#[tauri::command]
pub fn get_auth_token() -> Result<Option<String>, String> {
    // First check the new token refresh state
    if let Some(token) = token_refresh::get_valid_access_token() {
        return Ok(Some(token));
    }
    
    // Fallback to legacy session
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
        user_profile: None,
    });
    Ok(())
}

/// Clear authentication token
#[tauri::command]
pub async fn clear_auth_token() -> Result<(), String> {
    {
        let mut session = AUTH_SESSION
            .lock()
            .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
        *session = None;
    }
    clear_token().await?;
    token_refresh::clear_token_state();
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

#[tauri::command]
pub fn auth_get_state() -> Result<AuthStateResponse, String> {
    let session = AUTH_SESSION
        .lock()
        .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;

    // If we have a legacy session, use it
    if let Some(session) = session.as_ref() {
        return Ok(build_auth_state_response(Some(session)));
    }

    // Otherwise check if we have a valid token from silent refresh
    if let Some(_token_state) = token_refresh::get_token_state() {
        // Return authenticated state even without profile (it will be fetched asynchronously)
        return Ok(AuthStateResponse {
            is_authenticated: true,
            user: None,
        });
    }

    Ok(build_auth_state_response(None))
}

/// Populate AUTH_SESSION after successful silent refresh
/// Called by lib.rs setup to bridge silent refresh to legacy auth state
pub async fn populate_session_after_silent_refresh(
    app: &AppHandle,
) -> Result<(), String> {
    // Get current token state from silent refresh
    let token_state = token_refresh::get_token_state()
        .ok_or_else(|| "No token state available from silent refresh".to_string())?;

    // Try to fetch user profile from /userinfo endpoint
    let user_profile = match fetch_user_profile_from_userinfo(&token_state.access_token).await {
        Ok(profile) => Some(profile),
        Err(e) => {
            log::warn!("Failed to fetch user profile from userinfo: {}, falling back to id_token decode", e);
            // Try to decode from id_token if available in stored state (from previous login)
            // For now, we'll proceed without profile - the frontend can trigger a profile fetch
            None
        }
    };

    // Populate AUTH_SESSION with token and profile
    let session = {
        let mut auth_session = AUTH_SESSION
            .lock()
            .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
        let session = AuthSession {
            access_token: token_state.access_token.clone(),
            user_profile: user_profile.clone(),
        };
        *auth_session = Some(session.clone());
        session
    };

    let state = build_auth_state_response(Some(&session));
    emit_auth_state_changed(app, &state);

    log::info!("Populated AUTH_SESSION after silent refresh");
    Ok(())
}

#[tauri::command]
pub async fn auth_login(app: AppHandle) -> Result<(), String> {
    initiate_oauth_login(app).await
}

#[tauri::command]
pub async fn auth_logout(app: AppHandle) -> Result<(), String> {
    // Best-effort remote revocation/logout; local state clear must still happen
    let _ = token_refresh::logout().await;

    {
        let mut session = AUTH_SESSION
            .lock()
            .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
        *session = None;
    }

    clear_token().await?;
    token_refresh::clear_token_state();

    let state = AuthStateResponse {
        is_authenticated: false,
        user: None,
    };
    emit_auth_state_changed(&app, &state);

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

    // Store state + code_verifier in AuthCallbackManager for CSRF validation
    // (used by both deep link handler and dev server)
    let manager: tauri::State<'_, Arc<AuthCallbackManager>> = app.state();
    manager.set_pending_state(pkce.state.clone(), pkce.code_verifier.clone());

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
    app: AppHandle,
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

    // Store in legacy session
    let session = {
        let mut auth_session = AUTH_SESSION
            .lock()
            .map_err(|e: std::sync::PoisonError<_>| e.to_string())?;
        let session = AuthSession {
            access_token: token_response.access_token.clone(),
            user_profile: user_profile.clone(),
        };
        *auth_session = Some(session.clone());
        session
    };

    // Store refresh token
    if let Some(refresh_token) = token_response.refresh_token.as_deref() {
        store_token(refresh_token).await?;
        log::info!("Stored refresh token in secure storage ({} chars)", refresh_token.len());
    } else {
        log::warn!("Clerk did NOT return a refresh_token — session will not persist across restarts. Verify offline_access scope is enabled in the Clerk dashboard.");
    }

    // Update new token state with expiry tracking
    let expires_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + token_response.expires_in;
    let token_state = TokenState {
        access_token: token_response.access_token,
        refresh_token: token_response.refresh_token.unwrap_or_default(),
        expires_at_unix,
        user_id: user_profile.as_ref().map(|p| p.sub.clone()),
    };
    token_refresh::set_token_state(token_state);

    let state = build_auth_state_response(Some(&session));
    emit_auth_state_changed(&app, &state);

    user_profile.ok_or_else(|| "Missing id_token in token response".to_string())
}

/// Fetch user profile from Clerk /userinfo endpoint using access token
async fn fetch_user_profile_from_userinfo(access_token: &str) -> Result<UserProfile, String> {
    let client = reqwest::Client::new();
    let userinfo_url = format!("https://{}/userinfo", crate::auth::CLERK_DOMAIN);

    let resp = client
        .get(&userinfo_url)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .map_err(|e| format!("Userinfo request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(format!("Userinfo endpoint returned {}: {}", status, body));
    }

    #[derive(Debug, Deserialize)]
    struct UserInfoResponse {
        sub: String,
        name: Option<String>,
        email: Option<String>,
        picture: Option<String>,
    }

    let user_info: UserInfoResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse userinfo response: {}", e))?;

    Ok(UserProfile {
        sub: user_info.sub,
        name: user_info.name,
        email: user_info.email,
        picture: user_info.picture,
    })
}

/// Decode id_token to extract user profile
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

// Token refresh commands

/// Perform token refresh (can be called explicitly or happens automatically)
/// Returns the new token state if successful, None if no token exists, or error if refresh failed
#[tauri::command]
pub async fn perform_token_refresh() -> Result<Option<TokenState>, String> {
    match token_refresh::perform_silent_refresh().await {
        RefreshResult::Success(state) => Ok(Some(state)),
        RefreshResult::NoToken => Ok(None),
        RefreshResult::Failed(e) => Err(e),
    }
}

/// Get current authentication state including expiry information
#[tauri::command]
pub fn get_auth_state() -> Result<Option<TokenState>, String> {
    Ok(token_refresh::get_token_state())
}

/// Check if the current token is expired or about to expire (within 30s buffer)
#[tauri::command]
pub fn is_token_expired() -> Result<bool, String> {
    Ok(token_refresh::is_token_expired_or_stale())
}

/// Logout - clear all authentication state
#[tauri::command]
pub async fn logout() -> Result<(), String> {
    token_refresh::logout().await
}

/// Check if a git repository has an 'origin' remote configured
#[tauri::command]
pub fn check_git_remote(project_path: String) -> Result<Option<String>, String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git: {}", e))?;

    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if url.is_empty() {
            Ok(None)
        } else {
            Ok(Some(url))
        }
    } else {
        Ok(None)
    }
}

/// Set up a git remote and push the current branch
#[tauri::command]
pub fn setup_git_remote(project_path: String, remote_url: String) -> Result<String, String> {
    // Add the origin remote
    let add_output = std::process::Command::new("git")
        .args(["remote", "add", "origin", &remote_url])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git remote add: {}", e))?;

    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr).trim().to_string();
        return Err(format!("git remote add failed: {}", stderr));
    }

    // Get the current branch name
    let branch_output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to get current branch: {}", e))?;

    let branch = if branch_output.status.success() {
        String::from_utf8_lossy(&branch_output.stdout).trim().to_string()
    } else {
        "main".to_string()
    };

    // Push and set upstream
    let push_output = std::process::Command::new("git")
        .args(["push", "-u", "origin", &branch])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git push: {}", e))?;

    if !push_output.status.success() {
        let stderr = String::from_utf8_lossy(&push_output.stderr).trim().to_string();
        return Err(format!("git push failed: {}", stderr));
    }

    Ok(format!("Remote configured and pushed to origin/{}", branch))
}

/// Open a native directory picker dialog
#[tauri::command]
pub async fn select_directory(
    window: tauri::Window,
    title: Option<String>,
) -> Result<Option<std::path::PathBuf>, String> {
    use tauri_plugin_dialog::DialogExt;

    let folder_path = window
        .dialog()
        .file()
        .set_title(title.as_deref().unwrap_or("Select Directory"))
        .blocking_pick_folder();

    folder_path.map(|p| p.into_path().map_err(|e| e.to_string())).transpose()
}
