use crate::auth::{clear_token, retrieve_token, store_token};
use crate::server::ServerState;
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

use serde::Deserialize;
use std::sync::Mutex as StdMutex;
use tauri::{AppHandle, Emitter, State};

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

/// Response from start_github_login containing the device code info for the frontend
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeInfo {
    pub user_code: String,
    pub verification_uri: String,
}

/// Start GitHub device code login flow.
///
/// Returns { userCode, verificationUri } for the frontend to display.
/// Spawns a background task that polls for authorization, stores tokens,
/// fetches user profile, populates AUTH_SESSION, and emits auth:state-changed.
#[tauri::command]
pub async fn start_github_login(app: AppHandle) -> Result<DeviceCodeInfo, String> {
    let device_resp = crate::auth::start_device_flow().await?;

    let info = DeviceCodeInfo {
        user_code: device_resp.user_code.clone(),
        verification_uri: device_resp.verification_uri.clone(),
    };

    // Spawn background polling task
    let device_code = device_resp.device_code.clone();
    let interval = device_resp.interval;
    tauri::async_runtime::spawn(async move {
        match crate::auth::poll_device_flow(&device_code, interval).await {
            Ok(token_response) => {
                let now_unix = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let expires_in = token_response.expires_in.unwrap_or(28800);
                let expires_at_unix = now_unix + expires_in;

                // Fetch user profile
                let github_user =
                    match crate::auth::fetch_github_user(&token_response.access_token).await {
                        Ok(user) => Some(user),
                        Err(e) => {
                            log::warn!("Failed to fetch GitHub user profile: {}", e);
                            None
                        }
                    };

                let user_profile = github_user.as_ref().map(|u| UserProfile {
                    sub: u.id.to_string(),
                    name: u.name.clone().or_else(|| Some(u.login.clone())),
                    email: u.email.clone(),
                    picture: Some(u.avatar_url.clone()),
                });

                // Store tokens as JSON blob in keyring
                let refresh_token = token_response.refresh_token.clone().unwrap_or_default();
                if let Some(ref gh_user) = github_user {
                    let stored = crate::auth::StoredTokens {
                        access_token: token_response.access_token.clone(),
                        refresh_token: refresh_token.clone(),
                        expires_at: expires_at_unix,
                        user_login: gh_user.login.clone(),
                        avatar_url: gh_user.avatar_url.clone(),
                    };
                    if let Ok(json) = serde_json::to_string(&stored) {
                        if let Err(e) = store_token(&json).await {
                            log::error!("Failed to store tokens in keyring: {}", e);
                        }
                    }
                } else if !refresh_token.is_empty() {
                    // Store minimal token data
                    let stored = crate::auth::StoredTokens {
                        access_token: token_response.access_token.clone(),
                        refresh_token: refresh_token.clone(),
                        expires_at: expires_at_unix,
                        user_login: String::new(),
                        avatar_url: String::new(),
                    };
                    if let Ok(json) = serde_json::to_string(&stored) {
                        if let Err(e) = store_token(&json).await {
                            log::error!("Failed to store tokens in keyring: {}", e);
                        }
                    }
                }

                // Sync tokens to server credential vault
                {
                    let at = token_response.access_token.clone();
                    let rt = refresh_token.clone();
                    let ul = github_user.as_ref().map(|u| u.login.clone());
                    crate::token_sync::sync_tokens_to_server(
                        &at,
                        &rt,
                        expires_at_unix,
                        ul.as_deref(),
                    )
                    .await;
                }

                // Update token refresh state
                let token_state = TokenState {
                    access_token: token_response.access_token.clone(),
                    refresh_token,
                    expires_at_unix,
                    user_id: github_user.as_ref().map(|u| u.id.to_string()),
                };
                token_refresh::set_token_state(token_state);

                // Populate AUTH_SESSION
                let session = {
                    let mut auth_session = AUTH_SESSION.lock().unwrap();
                    let session = AuthSession {
                        access_token: token_response.access_token,
                        user_profile: user_profile.clone(),
                    };
                    *auth_session = Some(session.clone());
                    session
                };

                let state = build_auth_state_response(Some(&session));
                emit_auth_state_changed(&app, &state);

                log::info!("GitHub device flow login successful");
            }
            Err(e) => {
                log::error!("GitHub device flow polling failed: {}", e);
                let _ = app.emit(
                    "auth:login-failed",
                    serde_json::json!({
                        "reason": e,
                    }),
                );
            }
        }
    });

    Ok(info)
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
pub async fn populate_session_after_silent_refresh(app: &AppHandle) -> Result<(), String> {
    // Get current token state from silent refresh
    let token_state = token_refresh::get_token_state()
        .ok_or_else(|| "No token state available from silent refresh".to_string())?;

    // Try to fetch user profile from GitHub API
    let user_profile = match crate::auth::fetch_github_user(&token_state.access_token).await {
        Ok(github_user) => Some(UserProfile {
            sub: github_user.id.to_string(),
            name: github_user.name.or_else(|| Some(github_user.login.clone())),
            email: github_user.email,
            picture: Some(github_user.avatar_url),
        }),
        Err(e) => {
            log::warn!(
                "Failed to fetch GitHub user profile after silent refresh: {}",
                e
            );
            // Try to get user info from stored tokens
            match retrieve_token().await {
                Ok(Some(stored_json)) => {
                    if let Ok(stored) =
                        serde_json::from_str::<crate::auth::StoredTokens>(&stored_json)
                    {
                        if !stored.user_login.is_empty() {
                            Some(UserProfile {
                                sub: stored.user_login.clone(),
                                name: Some(stored.user_login),
                                email: None,
                                picture: if stored.avatar_url.is_empty() {
                                    None
                                } else {
                                    Some(stored.avatar_url)
                                },
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            }
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
    // Start GitHub device flow - the frontend should use start_github_login instead,
    // but this wrapper maintains backwards compatibility with the auth store
    let _info = start_github_login(app).await?;
    Ok(())
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
        let stderr = String::from_utf8_lossy(&add_output.stderr)
            .trim()
            .to_string();
        return Err(format!("git remote add failed: {}", stderr));
    }

    // Get the current branch name
    let branch_output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to get current branch: {}", e))?;

    let branch = if branch_output.status.success() {
        String::from_utf8_lossy(&branch_output.stdout)
            .trim()
            .to_string()
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
        let stderr = String::from_utf8_lossy(&push_output.stderr)
            .trim()
            .to_string();
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

    folder_path
        .map(|p| p.into_path().map_err(|e| e.to_string()))
        .transpose()
}
