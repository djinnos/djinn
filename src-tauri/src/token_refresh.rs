//! Silent token refresh with rotation serialization
//!
//! Implements:
//! - Mutex-based serialization of concurrent refresh calls
//! - 30-second expiry buffer before refreshing
//! - Token rotation handling (Clerk may return new refresh token)
//! - Automatic cleanup on refresh failure

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use crate::auth::{clear_token, retrieve_token, store_token, CLERK_DOMAIN};

/// Token endpoint path
const TOKEN_ENDPOINT: &str = "/oauth/token";

/// Client ID for OAuth
const CLIENT_ID: &str = "djinnos-desktop";

/// Expiry buffer: refresh tokens 30 seconds before actual expiry
const EXPIRY_BUFFER_SECONDS: u64 = 30;

/// Response from Clerk token endpoint
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

/// Current authentication state with expiry tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenState {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp (seconds) when the token expires
    pub expires_at_unix: u64,
    pub user_id: Option<String>,
}

/// Result of a token refresh attempt
#[derive(Debug, Clone)]
pub enum RefreshResult {
    Success(TokenState),
    NoToken,
    Failed(String),
}

/// Global mutex for serializing concurrent refresh calls
static REFRESH_MUTEX: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));

/// Global storage for current token state
static CURRENT_TOKEN_STATE: Lazy<Mutex<Option<TokenState>>> = Lazy::new(|| Mutex::new(None));

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Check if the current token is expired or about to expire (within buffer)
pub fn is_token_expired_or_stale() -> bool {
    let state = CURRENT_TOKEN_STATE.lock().unwrap();
    match state.as_ref() {
        None => true,
        Some(token_state) => now_unix() + EXPIRY_BUFFER_SECONDS >= token_state.expires_at_unix,
    }
}

/// Get the current access token if valid (not expired or stale)
pub fn get_valid_access_token() -> Option<String> {
    let state = CURRENT_TOKEN_STATE.lock().unwrap();
    state.as_ref().and_then(|token_state| {
        if now_unix() + EXPIRY_BUFFER_SECONDS < token_state.expires_at_unix {
            Some(token_state.access_token.clone())
        } else {
            None
        }
    })
}

/// Get the current token state (cloned)
pub fn get_token_state() -> Option<TokenState> {
    CURRENT_TOKEN_STATE.lock().unwrap().clone()
}

/// Set the current token state
pub fn set_token_state(state: TokenState) {
    let mut current = CURRENT_TOKEN_STATE.lock().unwrap();
    *current = Some(state);
}

/// Clear the current token state (logout)
pub fn clear_token_state() {
    let mut current = CURRENT_TOKEN_STATE.lock().unwrap();
    *current = None;
}

/// Perform silent token refresh using stored refresh token
/// 
/// This function is serialized via a global mutex to prevent concurrent
/// refresh calls from causing race conditions or token rotation issues.
pub async fn perform_silent_refresh() -> RefreshResult {
    // Acquire mutex to serialize concurrent refresh calls
    let _guard = REFRESH_MUTEX.lock().await;
    
    // Double-check after acquiring lock - another thread may have refreshed
    if let Some(_token) = get_valid_access_token() {
        log::debug!("Token still valid after acquiring refresh lock, skipping refresh");
        let state = CURRENT_TOKEN_STATE.lock().unwrap();
        if let Some(s) = state.clone() {
            return RefreshResult::Success(s);
        }
    }

    // Retrieve stored refresh token
    let refresh_token = match retrieve_token().await {
        Ok(Some(token)) => token,
        Ok(None) => {
            log::info!("No refresh token found in storage");
            return RefreshResult::NoToken;
        }
        Err(e) => {
            log::error!("Failed to retrieve refresh token: {}", e);
            return RefreshResult::Failed(format!("Failed to retrieve token: {}", e));
        }
    };

    // Call Clerk token endpoint
    let client = reqwest::Client::new();
    let token_url = format!("https://{}{}", CLERK_DOMAIN, TOKEN_ENDPOINT);

    log::info!("Performing silent token refresh");

    let resp = match client
        .post(&token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            log::error!("Token refresh request failed: {}", e);
            // Clear token on network failure
            let _ = clear_token().await;
            clear_token_state();
            return RefreshResult::Failed(format!("Network error: {}", e));
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable>".to_string());
        log::error!("Token refresh failed with status {}: {}", status, body);

        // Clear stored token on authentication failure
        let _ = clear_token().await;
        clear_token_state();

        return RefreshResult::Failed(format!("HTTP {}: {}", status, body));
    }

    let token_response: TokenResponse = match resp.json().await {
        Ok(tr) => tr,
        Err(e) => {
            log::error!("Failed to parse token response: {}", e);
            let _ = clear_token().await;
            clear_token_state();
            return RefreshResult::Failed(format!("Parse error: {}", e));
        }
    };

    // Calculate expiry time as unix timestamp
    let expires_at_unix = now_unix() + token_response.expires_in;

    // Handle token rotation: Clerk may return a new refresh token
    let had_new_refresh_token = token_response.refresh_token.is_some();
    let new_refresh_token = token_response.refresh_token.unwrap_or_else(|| {
        log::warn!("Clerk did not return new refresh token, reusing existing");
        refresh_token.clone()
    });

    // Store the new refresh token (handles rotation)
    if let Err(e) = store_token(&new_refresh_token).await {
        log::error!("Failed to store rotated refresh token: {}", e);
        // Continue anyway - we have a valid access token for now
    } else if had_new_refresh_token {
        log::info!("Stored rotated refresh token");
    }

    // Update token state
    let token_state = TokenState {
        access_token: token_response.access_token.clone(),
        refresh_token: new_refresh_token,
        expires_at_unix,
        user_id: None, // Extracted from id_token if needed
    };

    set_token_state(token_state.clone());

    log::info!("Silent token refresh successful, token expires in {}s", token_response.expires_in);

    RefreshResult::Success(token_state)
}

/// Check for stored refresh token on startup and attempt silent refresh
/// 
/// This should be called during app initialization to restore the user's
/// session without requiring re-authentication.
pub async fn attempt_silent_auth_on_startup() -> RefreshResult {
    log::info!("Checking for stored refresh token on startup");

    // Check if we have a refresh token stored
    match retrieve_token().await {
        Ok(Some(_)) => {
            log::info!("Found stored refresh token, attempting silent refresh");
            perform_silent_refresh().await
        }
        Ok(None) => {
            log::info!("No stored refresh token found, user needs to authenticate");
            RefreshResult::NoToken
        }
        Err(e) => {
            log::error!("Failed to check for stored token: {}", e);
            RefreshResult::Failed(format!("Storage error: {}", e))
        }
    }
}

/// Clear all authentication state (logout)
pub async fn logout() -> Result<(), String> {
    let _guard = REFRESH_MUTEX.lock().await;
    
    clear_token_state();
    clear_token().await?;
    
    log::info!("User logged out, all tokens cleared");
    Ok(())
}
