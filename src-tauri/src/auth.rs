use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use rand::distributions::Alphanumeric;

/// Authentication state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthState {
    pub token: Option<String>,
    pub user_id: Option<String>,
}

impl Default for AuthState {
    fn default() -> Self {
        Self {
            token: None,
            user_id: None,
        }
    }
}

/// PKCE parameters for OAuth flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkceParams {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

/// Clerk OAuth configuration
pub const CLERK_DOMAIN: &str = "clerk.djinnai.io";
pub const OAUTH_AUTHORIZE_PATH: &str = "/oauth/authorize";
pub const CLIENT_ID: &str = "djinnos-desktop";
pub const REDIRECT_URI: &str = "djinnos://auth/callback";

/// Generate PKCE code_verifier and code_challenge
///
/// Returns PKCE parameters including code_verifier, code_challenge (SHA256), and state
pub fn generate_pkce() -> PkceParams {
    let code_verifier = generate_code_verifier();
    let code_challenge = generate_code_challenge(&code_verifier);
    let state = generate_state();
    
    PkceParams {
        code_verifier,
        code_challenge,
        state,
    }
}

/// Generate a random code_verifier (43-128 chars)
///
/// Per RFC 7636, code_verifier is a random string using [A-Za-z0-9_-]
/// with length between 43 and 128 characters.
fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rng = rand::thread_rng();
    
    let verifier: String = (0..128)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    
    verifier
}

/// Generate code_challenge from code_verifier using SHA256
///
/// code_challenge = BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))
fn generate_code_challenge(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();
    
    URL_SAFE_NO_PAD.encode(&hash)
}

/// Generate random state parameter for CSRF protection
///
/// Returns a 32-byte random string encoded as URL-safe base64
fn generate_state() -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
    URL_SAFE_NO_PAD.encode(&bytes)
}

/// Build Clerk OAuth authorization URL with PKCE parameters
pub fn build_authorize_url(pkce: &PkceParams) -> String {
    use url::form_urlencoded;
    
    let scope = "openid profile email offline_access";
    let encoded_scope = form_urlencoded::byte_serialize(scope.as_bytes()).collect::<String>();
    let encoded_redirect = form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>();
    
    format!(
        "https://{}{}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&code_challenge={}&code_challenge_method=S256&prompt=login",
        CLERK_DOMAIN,
        OAUTH_AUTHORIZE_PATH,
        CLIENT_ID,
        encoded_redirect,
        encoded_scope,
        pkce.state,
        pkce.code_challenge
    )
}

/// Initialize authentication state
pub async fn init_auth() -> Result<AuthState, String> {
    Ok(AuthState::default())
}

/// Store authentication token securely
pub async fn store_token(_token: &str) -> Result<(), String> {
    Ok(())
}

/// Retrieve authentication token from secure storage
pub async fn retrieve_token() -> Result<Option<String>, String> {
    Ok(None)
}

/// Clear authentication token from secure storage
pub async fn clear_token() -> Result<(), String> {
    Ok(())
}

/// Handle deep link callback for OAuth PKCE
pub async fn handle_deep_link(_url: &str) -> Result<AuthState, String> {
    Ok(AuthState::default())
}

/// Check if user is authenticated
pub fn is_authenticated(state: &AuthState) -> bool {
    state.token.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_verifier_length() {
        let verifier = generate_code_verifier();
        assert!(verifier.len() >= 43);
        assert!(verifier.len() <= 128);
        assert!(verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_code_challenge_format() {
        let verifier = generate_code_verifier();
        let challenge = generate_code_challenge(&verifier);
        assert_eq!(challenge.len(), 43);
        assert!(challenge.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_state_length() {
        let state = generate_state();
        assert_eq!(state.len(), 43);
    }

    #[test]
    fn test_authorize_url_format() {
        let pkce = generate_pkce();
        let url = build_authorize_url(&pkce);
        
        assert!(url.starts_with("https://clerk.djinnai.io/oauth/authorize"));
        assert!(url.contains(&format!("state={}", pkce.state)));
        assert!(url.contains(&format!("code_challenge={}", pkce.code_challenge)));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=djinnos-desktop"));
        assert!(url.contains("scope=openid+profile+email+offline_access"));
        assert!(url.contains("prompt=login"));
    }
}
