use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

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
pub const REDIRECT_URI: &str = "djinn://auth/callback";
const KEYRING_SERVICE: &str = "djinnos-desktop";
const KEYRING_USERNAME: &str = "refresh_token";

/// Generate PKCE code_verifier and code_challenge
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

fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut rng = rand::thread_rng();

    (0..128)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

fn generate_code_challenge(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();

    URL_SAFE_NO_PAD.encode(hash)
}

fn generate_state() -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
    URL_SAFE_NO_PAD.encode(&bytes)
}

pub fn build_authorize_url(pkce: &PkceParams) -> String {
    use url::form_urlencoded;

    let scope = "openid profile email offline_access";
    let encoded_scope = form_urlencoded::byte_serialize(scope.as_bytes()).collect::<String>();
    let encoded_redirect =
        form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>();

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

fn token_hint_path() -> Result<PathBuf, String> {
    let mut dir = dirs::config_dir().ok_or_else(|| "Unable to resolve config dir".to_string())?;
    dir.push("djinnos");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create config dir: {e}"))?;
    dir.push("refresh_token.meta");
    Ok(dir)
}

fn set_owner_only_permissions(path: &PathBuf) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed setting 0o600 permissions: {e}"))?;
    }
    Ok(())
}

fn touch_hint_file() -> Result<(), String> {
    let hint_path = token_hint_path()?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&hint_path)
        .map_err(|e| format!("Failed to create token metadata file: {e}"))?;
    file.write_all(b"stored_in_keyring=true\n")
        .map_err(|e| format!("Failed to write token metadata file: {e}"))?;
    file.flush()
        .map_err(|e| format!("Failed to flush token metadata file: {e}"))?;
    set_owner_only_permissions(&hint_path)?;
    Ok(())
}

fn delete_hint_file() -> Result<(), String> {
    let hint_path = token_hint_path()?;
    if hint_path.exists() {
        fs::remove_file(&hint_path).map_err(|e| format!("Failed to remove token metadata file: {e}"))?;
    }
    Ok(())
}

pub async fn store_token(token: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
        .map_err(|e| format!("Failed to initialize keyring entry: {e}"))?;
    entry
        .set_password(token)
        .map_err(|e| format!("Failed to store refresh token in keyring: {e}"))?;
    touch_hint_file()?;
    Ok(())
}

pub async fn retrieve_token() -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
        .map_err(|e| format!("Failed to initialize keyring entry: {e}"))?;

    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("Failed to retrieve refresh token from keyring: {e}")),
    }
}

pub async fn clear_token() -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
        .map_err(|e| format!("Failed to initialize keyring entry: {e}"))?;

    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            delete_hint_file()?;
            Ok(())
        }
        Err(e) => Err(format!("Failed to clear refresh token from keyring: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_pkce_produces_valid_length_and_chars() {
        let pkce = generate_pkce();

        // code_verifier should be 128 characters long
        assert_eq!(pkce.code_verifier.len(), 128, "code_verifier should be 128 characters");

        // code_verifier should only contain valid characters: A-Z, a-z, 0-9, -_
        let valid_chars: Vec<char> = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_".chars().collect();
        for c in pkce.code_verifier.chars() {
            assert!(
                valid_chars.contains(&c),
                "code_verifier contains invalid character: {}",
                c
            );
        }

        // code_challenge should be valid base64url (no padding)
        // SHA256 produces 32 bytes, base64url encodes to ~43 chars
        assert!(
            pkce.code_challenge.len() >= 43,
            "code_challenge should be at least 43 characters"
        );

        // state should be present and non-empty
        assert!(!pkce.state.is_empty(), "state should not be empty");
    }

    #[test]
    fn test_code_challenge_is_sha256_of_verifier() {
        let pkce = generate_pkce();

        // Manually compute SHA256 hash of code_verifier
        let mut hasher = Sha256::new();
        hasher.update(pkce.code_verifier.as_bytes());
        let expected_hash = hasher.finalize();
        let expected_challenge = URL_SAFE_NO_PAD.encode(expected_hash);

        assert_eq!(
            pkce.code_challenge, expected_challenge,
            "code_challenge should be SHA256 of code_verifier"
        );
    }

    #[test]
    fn test_build_authorize_url_contains_all_required_params() {
        let pkce = generate_pkce();
        let url = build_authorize_url(&pkce);

        // Check that URL starts with https://
        assert!(url.starts_with("https://"), "URL should start with https://");

        // Check domain
        assert!(
            url.contains(&format!("https://{}", CLERK_DOMAIN)),
            "URL should contain the Clerk domain"
        );

        // Check path
        assert!(
            url.contains(OAUTH_AUTHORIZE_PATH),
            "URL should contain the authorize path"
        );

        // Check required parameters
        assert!(url.contains("client_id="), "URL should contain client_id");
        assert!(url.contains(CLIENT_ID), "URL should contain the correct client_id");
        assert!(url.contains("redirect_uri="), "URL should contain redirect_uri");
        assert!(url.contains("response_type=code"), "URL should contain response_type=code");
        assert!(url.contains("scope="), "URL should contain scope");
        assert!(url.contains("state="), "URL should contain state");
        assert!(url.contains(&pkce.state), "URL should contain the correct state");
        assert!(url.contains("code_challenge="), "URL should contain code_challenge");
        assert!(
            url.contains(&pkce.code_challenge),
            "URL should contain the correct code_challenge"
        );
        assert!(
            url.contains("code_challenge_method=S256"),
            "URL should contain code_challenge_method=S256"
        );
        assert!(url.contains("prompt=login"), "URL should contain prompt=login");
    }

    #[test]
    fn test_build_authorize_url_contains_correct_redirect_uri() {
        let pkce = generate_pkce();
        let url = build_authorize_url(&pkce);

        // The redirect_uri should be URL-encoded in the query string
        let encoded_redirect =
            url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect::<String>();
        assert!(
            url.contains(&format!("redirect_uri={}", encoded_redirect)),
            "URL should contain the correct encoded redirect_uri"
        );
    }

    #[test]
    fn test_redirect_uri_starts_with_scheme_from_tauri_conf() {
        // From tauri.conf.json, the scheme is "djinn"
        // REDIRECT_URI should start with djinn://
        assert!(
            REDIRECT_URI.starts_with("djinn://"),
            "REDIRECT_URI should start with djinn:// scheme from tauri.conf.json"
        );
    }
}

