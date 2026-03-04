//! Authentication callback handling for OAuth PKCE
//!
//! Handles both:
//! - Production: djinn://auth/callback via tauri-plugin-deep-link
//! - Dev mode: http://localhost:19876/auth/callback via ephemeral HTTP server

use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

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

/// Generate PKCE code verifier and challenge
pub fn generate_pkce() -> (String, String) {
    // Generate 32 random bytes for code_verifier
    let verifier_bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let code_verifier = URL_SAFE_NO_PAD.encode(&verifier_bytes);
    
    // Generate code_challenge = SHA256(code_verifier) base64url encoded
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let challenge = hasher.finalize();
    let code_challenge = URL_SAFE_NO_PAD.encode(&challenge);
    
    (code_verifier, code_challenge)
}

/// Generate random state parameter for CSRF protection
pub fn generate_state() -> String {
    let state_bytes: Vec<u8> = (0..16).map(|_| rand::random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(&state_bytes)
}
