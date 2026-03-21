//! Silent token refresh with rotation serialization
//!
//! Implements:
//! - Mutex-based serialization of concurrent refresh calls
//! - 30-second expiry buffer before refreshing
//! - Token rotation handling (GitHub returns new refresh token)
//! - Automatic cleanup on refresh failure

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use crate::auth::{
    clear_token, retrieve_token, store_token, StoredTokens, ACCESS_TOKEN_URL, GITHUB_CLIENT_ID,
};

/// Expiry buffer: refresh tokens 30 seconds before actual expiry
const EXPIRY_BUFFER_SECONDS: u64 = 30;

/// Response from GitHub token endpoint
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
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

    // Retrieve stored token blob from keyring
    let stored_json = match retrieve_token().await {
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

    // Parse stored tokens JSON
    let stored_tokens: StoredTokens = match serde_json::from_str(&stored_json) {
        Ok(t) => t,
        Err(e) => {
            log::error!("Failed to parse stored tokens: {}", e);
            let _ = clear_token().await;
            clear_token_state();
            return RefreshResult::Failed(format!("Invalid stored token format: {}", e));
        }
    };

    // GitHub OAuth App tokens don't have refresh tokens — they never expire.
    // If no refresh token is stored, the access token is still valid.
    // Restore the session from the stored access token directly.
    if stored_tokens.refresh_token.is_empty() {
        log::info!("No refresh token (OAuth App token) — restoring session from stored access token");
        let state = TokenState {
            access_token: stored_tokens.access_token.clone(),
            refresh_token: String::new(),
            // OAuth App tokens don't expire — use a far-future timestamp (1 year).
            expires_at_unix: now_unix() + 365 * 24 * 3600,
            user_id: None,
        };
        set_token_state(state.clone());
        return RefreshResult::Success(state);
    }

    // Call GitHub token endpoint
    let client = reqwest::Client::new();

    log::info!("Performing silent token refresh via GitHub");

    let resp = match client
        .post(ACCESS_TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", stored_tokens.refresh_token.as_str()),
            ("client_id", GITHUB_CLIENT_ID),
        ])
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            log::error!("Token refresh request failed: {}", e);
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
    let expires_in = token_response.expires_in.unwrap_or(28800);
    let expires_at_unix = now_unix() + expires_in;

    // Handle token rotation: GitHub returns a new refresh token
    let new_refresh_token = token_response.refresh_token.unwrap_or_else(|| {
        log::warn!("GitHub did not return new refresh token, reusing existing");
        stored_tokens.refresh_token.clone()
    });

    // Update stored tokens in keyring
    let updated_stored = StoredTokens {
        access_token: token_response.access_token.clone(),
        refresh_token: new_refresh_token.clone(),
        expires_at: expires_at_unix,
        user_login: stored_tokens.user_login,
        avatar_url: stored_tokens.avatar_url,
    };

    if let Ok(json) = serde_json::to_string(&updated_stored) {
        if let Err(e) = store_token(&json).await {
            log::error!("Failed to store refreshed tokens: {}", e);
            // Continue anyway - we have a valid access token for now
        } else {
            log::info!("Stored refreshed tokens");
        }
    }

    // Update token state
    let token_state = TokenState {
        access_token: token_response.access_token.clone(),
        refresh_token: new_refresh_token,
        expires_at_unix,
        user_id: None,
    };

    set_token_state(token_state.clone());

    // Sync refreshed tokens to server credential vault
    crate::token_sync::sync_tokens_to_server(
        &token_state.access_token,
        &token_state.refresh_token,
        expires_at_unix,
        // user_login not available during refresh; use stored value if present
        if updated_stored.user_login.is_empty() {
            None
        } else {
            Some(updated_stored.user_login.as_str())
        },
    )
    .await;

    log::info!(
        "Silent token refresh successful, token expires in {}s",
        expires_in
    );

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

#[cfg(test)]
mod tests {
    use super::*;

    fn create_token_state(expires_in_seconds: u64) -> TokenState {
        TokenState {
            access_token: "test_access_token".to_string(),
            refresh_token: "test_refresh_token".to_string(),
            expires_at_unix: now_unix() + expires_in_seconds,
            user_id: Some("user_123".to_string()),
        }
    }

    #[test]
    fn test_is_token_expired_or_stale_returns_true_when_no_token() {
        // Clear any existing token state
        clear_token_state();

        assert!(
            is_token_expired_or_stale(),
            "Should return true when no token state exists"
        );
    }

    #[test]
    fn test_is_token_expired_or_stale_returns_false_for_future_token() {
        // Token expires in 1 hour (well beyond the 30-second buffer)
        let state = create_token_state(3600);
        set_token_state(state);

        assert!(
            !is_token_expired_or_stale(),
            "Should return false for token expiring in the future"
        );
    }

    #[test]
    fn test_is_token_expired_or_stale_returns_true_for_past_token() {
        // Token expired 1 hour ago
        let state = TokenState {
            access_token: "test_access_token".to_string(),
            refresh_token: "test_refresh_token".to_string(),
            expires_at_unix: now_unix().saturating_sub(3600),
            user_id: Some("user_123".to_string()),
        };
        set_token_state(state);

        assert!(
            is_token_expired_or_stale(),
            "Should return true for already expired token"
        );
    }

    #[test]
    fn test_is_token_expired_or_stale_returns_true_at_buffer_edge() {
        // Token expires in exactly EXPIRY_BUFFER_SECONDS - 1 seconds
        // This should trigger staleness because we add buffer to current time
        let state = create_token_state(EXPIRY_BUFFER_SECONDS - 1);
        set_token_state(state);

        assert!(
            is_token_expired_or_stale(),
            "Should return true when token is within the expiry buffer"
        );
    }

    #[test]
    fn test_is_token_expired_or_stale_returns_false_just_outside_buffer() {
        // Token expires in EXPIRY_BUFFER_SECONDS + 5 seconds
        // This should be fresh
        let state = create_token_state(EXPIRY_BUFFER_SECONDS + 5);
        set_token_state(state);

        assert!(
            !is_token_expired_or_stale(),
            "Should return false when token is outside the expiry buffer"
        );
    }

    #[test]
    fn test_set_and_get_token_state() {
        clear_token_state();

        let state = create_token_state(3600);
        set_token_state(state.clone());

        let retrieved = get_token_state();
        assert!(retrieved.is_some(), "Should retrieve token state");

        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.access_token, state.access_token);
        assert_eq!(retrieved.refresh_token, state.refresh_token);
        assert_eq!(retrieved.expires_at_unix, state.expires_at_unix);
        assert_eq!(retrieved.user_id, state.user_id);
    }

    #[test]
    fn test_get_token_state_returns_none_when_empty() {
        clear_token_state();

        let retrieved = get_token_state();
        assert!(
            retrieved.is_none(),
            "Should return None when no token state set"
        );
    }

    #[test]
    fn test_clear_token_state() {
        let state = create_token_state(3600);
        set_token_state(state);

        assert!(get_token_state().is_some(), "Token state should exist");

        clear_token_state();

        assert!(get_token_state().is_none(), "Token state should be cleared");
    }

    #[test]
    fn test_get_valid_access_token_returns_token_when_valid() {
        // Token expires in 1 hour
        let state = create_token_state(3600);
        set_token_state(state.clone());

        let token = get_valid_access_token();
        assert!(token.is_some(), "Should return access token when valid");
        assert_eq!(token.unwrap(), state.access_token);
    }

    #[test]
    fn test_get_valid_access_token_returns_none_when_expired() {
        // Token expired 1 hour ago
        let state = TokenState {
            access_token: "test_access_token".to_string(),
            refresh_token: "test_refresh_token".to_string(),
            expires_at_unix: now_unix().saturating_sub(3600),
            user_id: Some("user_123".to_string()),
        };
        set_token_state(state);

        let token = get_valid_access_token();
        assert!(token.is_none(), "Should return None when token is expired");
    }

    #[test]
    fn test_get_valid_access_token_returns_none_within_buffer() {
        // Token expires in 15 seconds (within 30-second buffer)
        let state = create_token_state(15);
        set_token_state(state);

        let token = get_valid_access_token();
        assert!(
            token.is_none(),
            "Should return None when token is within expiry buffer"
        );
    }

    #[test]
    fn test_token_state_overwrite() {
        clear_token_state();

        let state1 = create_token_state(3600);
        set_token_state(state1.clone());

        let state2 = TokenState {
            access_token: "new_access_token".to_string(),
            refresh_token: "new_refresh_token".to_string(),
            expires_at_unix: now_unix() + 7200,
            user_id: Some("user_456".to_string()),
        };
        set_token_state(state2.clone());

        let retrieved = get_token_state().unwrap();
        assert_eq!(retrieved.access_token, "new_access_token");
        assert_eq!(retrieved.refresh_token, "new_refresh_token");
    }
}
