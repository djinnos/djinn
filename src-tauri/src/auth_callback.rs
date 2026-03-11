//! Authentication callback handling for OAuth PKCE
//!
//! Handles both:
//! - Production: djinn://auth/callback via tauri-plugin-deep-link
//! - Dev mode: http://localhost:19876/auth/callback via ephemeral HTTP server

use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

/// Port for dev-mode callback server
pub const DEV_CALLBACK_PORT: u16 = 19876;

/// OAuth callback data extracted from redirect
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCallbackData {
    pub code: String,
    pub state: String,
}

/// Pending OAuth state for CSRF validation
#[derive(Debug, Clone)]
pub struct PendingOAuthState {
    pub state: String,
    pub code_verifier: String,
}

/// Auth callback manager handles both deep links and dev-mode HTTP server
#[derive(Debug)]
pub struct AuthCallbackManager {
    /// Pending OAuth state (state + code_verifier) for CSRF validation
    pending_state: Arc<Mutex<Option<PendingOAuthState>>>,
    /// Channel sender for internal communication (used by dev server)
    callback_tx: Arc<Mutex<Option<mpsc::UnboundedSender<OAuthCallbackData>>>>,
}

impl AuthCallbackManager {
    /// Create a new auth callback manager
    pub fn new() -> Self {
        Self {
            pending_state: Arc::new(Mutex::new(None)),
            callback_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Set the pending OAuth state before initiating login
    pub fn set_pending_state(&self, state: String, code_verifier: String) {
        let mut pending = self.pending_state.lock().unwrap();
        *pending = Some(PendingOAuthState {
            state,
            code_verifier,
        });
    }

    /// Clear the pending OAuth state
    pub fn clear_pending_state(&self) {
        let mut pending = self.pending_state.lock().unwrap();
        *pending = None;
    }

    /// Get the pending code_verifier if state matches
    pub fn validate_state_and_get_verifier(&self, state: &str) -> Option<String> {
        let pending = self.pending_state.lock().unwrap();
        pending.as_ref().and_then(|s| {
            if s.state == state {
                Some(s.code_verifier.clone())
            } else {
                None
            }
        })
    }

    /// Handle a callback URL (from deep link or HTTP server)
    /// Returns the extracted OAuth data if valid
    pub fn handle_callback_url(&self, url: &str) -> Result<OAuthCallbackData, String> {
        // Extract query string from URL
        let query = url.split('?').nth(1)
            .ok_or("No query string in callback URL")?;
        
        // Parse query parameters
        let mut code = None;
        let mut state = None;
        let mut error = None;
        let mut error_description = None;
        
        for pair in query.split('&') {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");
            let decoded_value = percent_decode(value);
            
            match key {
                "code" => code = Some(decoded_value),
                "state" => state = Some(decoded_value),
                "error" => error = Some(decoded_value),
                "error_description" => error_description = Some(decoded_value),
                _ => {}
            }
        }
        
        // Check for error
        if let Some(err) = error {
            let desc = error_description.unwrap_or_else(|| "no description".to_string());
            return Err(format!("OAuth error: {} - {}", err, desc));
        }
        
        // Extract authorization code
        let code = code.ok_or("No authorization code in callback")?;
        let state = state.ok_or("No state in callback")?;
        
        Ok(OAuthCallbackData { code, state })
    }

    /// Process callback data and emit event to frontend
    pub fn process_callback(&self, app: &AppHandle, data: OAuthCallbackData) -> Result<(), String> {
        // Validate state matches pending state
        let verifier = self.validate_state_and_get_verifier(&data.state)
            .ok_or("Invalid state parameter - possible CSRF attack")?;

        // Emit event to frontend with code and verifier
        app.emit("auth:callback-received", serde_json::json!({
            "code": data.code,
            "state": data.state,
            "code_verifier": verifier,
        })).map_err(|e| format!("Failed to emit callback event: {}", e))?;

        // Clear pending state
        self.clear_pending_state();

        log::info!("Auth callback processed successfully");
        Ok(())
    }

    /// Set the channel sender for dev server communication
    pub fn set_callback_sender(&self, tx: mpsc::UnboundedSender<OAuthCallbackData>) {
        let mut sender = self.callback_tx.lock().unwrap();
        *sender = Some(tx);
    }

    /// Get the callback sender (for dev server)
    pub fn get_callback_sender(&self) -> Option<mpsc::UnboundedSender<OAuthCallbackData>> {
        self.callback_tx.lock().unwrap().clone()
    }
}

impl Default for AuthCallbackManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Percent-decode a string (simple implementation)
fn percent_decode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().and_then(|c| c.to_digit(16));
            let h2 = chars.next().and_then(|c| c.to_digit(16));
            if let (Some(h1), Some(h2)) = (h1, h2) {
                result.push(((h1 << 4) | h2) as u8 as char);
            } else {
                result.push('%');
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_callback_url_extracts_code_and_state() {
        let manager = AuthCallbackManager::new();
        let url = "djinn://auth/callback?code=abc123&state=xyz789";

        let result = manager.handle_callback_url(url);
        assert!(result.is_ok(), "Should successfully extract callback data");

        let data = result.unwrap();
        assert_eq!(data.code, "abc123", "Should extract correct code");
        assert_eq!(data.state, "xyz789", "Should extract correct state");
    }

    #[test]
    fn test_handle_callback_url_rejects_missing_code() {
        let manager = AuthCallbackManager::new();
        // URL with state but no code
        let url = "djinn://auth/callback?state=xyz789";

        let result = manager.handle_callback_url(url);
        assert!(result.is_err(), "Should reject URL without code");
        assert!(
            result.unwrap_err().contains("No authorization code"),
            "Error should mention missing code"
        );
    }

    #[test]
    fn test_handle_callback_url_rejects_error_responses() {
        let manager = AuthCallbackManager::new();
        // OAuth error response
        let url = "djinn://auth/callback?error=access_denied&error_description=user+denied+access&state=xyz789";

        let result = manager.handle_callback_url(url);
        assert!(result.is_err(), "Should reject OAuth error responses");
        let err = result.unwrap_err();
        assert!(
            err.contains("OAuth error"),
            "Error should indicate OAuth error"
        );
        assert!(
            err.contains("access_denied"),
            "Error should contain the error code"
        );
    }

    #[test]
    fn test_validate_state_and_get_verifier_matches_correct_state() {
        let manager = AuthCallbackManager::new();
        let state = "valid_state_123";
        let verifier = "my_code_verifier_abc";

        // Set pending state
        manager.set_pending_state(state.to_string(), verifier.to_string());

        // Validate with correct state
        let result = manager.validate_state_and_get_verifier(state);
        assert!(result.is_some(), "Should return verifier for correct state");
        assert_eq!(result.unwrap(), verifier, "Should return the correct verifier");
    }

    #[test]
    fn test_validate_state_and_get_verifier_rejects_wrong_state() {
        let manager = AuthCallbackManager::new();
        let state = "valid_state_123";
        let wrong_state = "wrong_state_456";
        let verifier = "my_code_verifier_abc";

        // Set pending state
        manager.set_pending_state(state.to_string(), verifier.to_string());

        // Validate with wrong state (CSRF attempt)
        let result = manager.validate_state_and_get_verifier(wrong_state);
        assert!(result.is_none(), "Should reject wrong state (CSRF protection)");
    }

    #[test]
    fn test_validate_state_when_no_pending_state() {
        let manager = AuthCallbackManager::new();

        // No pending state set
        let result = manager.validate_state_and_get_verifier("any_state");
        assert!(result.is_none(), "Should return None when no pending state");
    }

    #[test]
    fn test_clear_pending_state() {
        let manager = AuthCallbackManager::new();
        let state = "state_123";
        let verifier = "verifier_abc";

        // Set pending state
        manager.set_pending_state(state.to_string(), verifier.to_string());
        assert!(manager.validate_state_and_get_verifier(state).is_some(), "State should be set");

        // Clear pending state
        manager.clear_pending_state();
        assert!(manager.validate_state_and_get_verifier(state).is_none(), "State should be cleared");
    }

    #[test]
    fn test_handle_callback_url_with_url_encoded_values() {
        let manager = AuthCallbackManager::new();
        // URL with encoded values (space becomes +, special chars become %XX)
        let url = "djinn://auth/callback?code=code%20with%20spaces&state=state%2Bplus";

        let result = manager.handle_callback_url(url);
        assert!(result.is_ok(), "Should handle URL-encoded values");

        let data = result.unwrap();
        assert_eq!(data.code, "code with spaces", "Should decode URL-encoded code");
        assert_eq!(data.state, "state+plus", "Should decode URL-encoded state");
    }

    #[test]
    fn test_handle_callback_url_no_query_string() {
        let manager = AuthCallbackManager::new();
        let url = "djinn://auth/callback";

        let result = manager.handle_callback_url(url);
        assert!(result.is_err(), "Should reject URL without query string");
        assert!(
            result.unwrap_err().contains("No query string"),
            "Error should mention missing query string"
        );
    }
}

