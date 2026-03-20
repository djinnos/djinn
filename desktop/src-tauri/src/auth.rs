use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

/// GitHub OAuth App configuration
pub const GITHUB_CLIENT_ID: &str = "Iv23livjPjcHXVzAU7sc";
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const GITHUB_API_URL: &str = "https://api.github.com";
pub const GITHUB_SCOPES: &str = "repo read:org user:email";

const KEYRING_SERVICE: &str = "djinnos-desktop";
const KEYRING_USERNAME: &str = "refresh_token";

/// Response from GitHub device code endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Response from GitHub access token endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub refresh_token_expires_in: Option<u64>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// GitHub user profile from /user endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    pub id: u64,
    pub avatar_url: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// Stored token blob in keyring
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub user_login: String,
    pub avatar_url: String,
}

/// Error response from GitHub OAuth endpoints
#[derive(Debug, Deserialize)]
struct GitHubErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Start the GitHub device code flow
pub async fn start_device_flow() -> Result<DeviceCodeResponse, String> {
    let client = reqwest::Client::new();

    let resp = client
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", GITHUB_CLIENT_ID),
            ("scope", GITHUB_SCOPES),
        ])
        .send()
        .await
        .map_err(|e| format!("Device code request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(format!("Device code endpoint returned {}: {}", status, body));
    }

    resp.json::<DeviceCodeResponse>()
        .await
        .map_err(|e| format!("Failed to parse device code response: {}", e))
}

/// Poll GitHub for device flow authorization
pub async fn poll_device_flow(
    device_code: &str,
    interval: u64,
) -> Result<TokenResponse, String> {
    let client = reqwest::Client::new();
    let poll_interval = std::time::Duration::from_secs(interval.max(5));

    loop {
        tokio::time::sleep(poll_interval).await;

        let resp = client
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", GITHUB_CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|e| format!("Token poll request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(format!("Token endpoint returned {}: {}", status, body));
        }

        let body = resp.text().await.map_err(|e| format!("Failed to read response: {}", e))?;

        // Try to parse as error first
        if let Ok(err_resp) = serde_json::from_str::<GitHubErrorResponse>(&body) {
            match err_resp.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    // GitHub asks us to slow down; add 5 seconds
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                "expired_token" => return Err("Device code expired. Please try again.".to_string()),
                "access_denied" => return Err("Authorization was denied by the user.".to_string()),
                _ => {
                    let desc = err_resp.error_description.unwrap_or_default();
                    return Err(format!("OAuth error: {} - {}", err_resp.error, desc));
                }
            }
        }

        // Parse as successful token response
        let token_response: TokenResponse = serde_json::from_str(&body)
            .map_err(|e| format!("Failed to parse token response: {}", e))?;

        return Ok(token_response);
    }
}

/// Refresh an expired GitHub token
#[allow(dead_code)]
pub async fn refresh_github_token(refresh_token: &str) -> Result<TokenResponse, String> {
    let client = reqwest::Client::new();

    let resp = client
        .post(ACCESS_TOKEN_URL)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", GITHUB_CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|e| format!("Token refresh request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(format!("Token refresh returned {}: {}", status, body));
    }

    resp.json::<TokenResponse>()
        .await
        .map_err(|e| format!("Failed to parse refresh response: {}", e))
}

/// Fetch the authenticated GitHub user's profile
pub async fn fetch_github_user(access_token: &str) -> Result<GitHubUser, String> {
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/user", GITHUB_API_URL))
        .header("Authorization", format!("Bearer {}", access_token))
        .header("User-Agent", "djinnos-desktop")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GitHub user request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(format!("GitHub /user returned {}: {}", status, body));
    }

    resp.json::<GitHubUser>()
        .await
        .map_err(|e| format!("Failed to parse GitHub user: {}", e))
}

// --- Keyring storage ---

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
    fn test_github_constants_are_set() {
        assert_eq!(GITHUB_CLIENT_ID, "Iv23livjPjcHXVzAU7sc");
        assert!(DEVICE_CODE_URL.starts_with("https://github.com/"));
        assert!(ACCESS_TOKEN_URL.starts_with("https://github.com/"));
        assert!(GITHUB_API_URL.starts_with("https://api.github.com"));
        assert!(GITHUB_SCOPES.contains("repo"));
    }

    #[test]
    fn test_stored_tokens_serialization_roundtrip() {
        let tokens = StoredTokens {
            access_token: "ghu_abc123".to_string(),
            refresh_token: "ghr_def456".to_string(),
            expires_at: 1700000000,
            user_login: "testuser".to_string(),
            avatar_url: "https://avatars.githubusercontent.com/u/1".to_string(),
        };

        let json = serde_json::to_string(&tokens).expect("serialize");
        let parsed: StoredTokens = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.access_token, tokens.access_token);
        assert_eq!(parsed.refresh_token, tokens.refresh_token);
        assert_eq!(parsed.expires_at, tokens.expires_at);
        assert_eq!(parsed.user_login, tokens.user_login);
        assert_eq!(parsed.avatar_url, tokens.avatar_url);
    }

    #[test]
    fn test_device_code_response_deserialization() {
        let json = r#"{
            "device_code": "dc_123",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;

        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.device_code, "dc_123");
        assert_eq!(resp.user_code, "ABCD-1234");
        assert_eq!(resp.verification_uri, "https://github.com/login/device");
        assert_eq!(resp.expires_in, 900);
        assert_eq!(resp.interval, 5);
    }

    #[test]
    fn test_token_response_deserialization() {
        let json = r#"{
            "access_token": "ghu_abc",
            "token_type": "bearer",
            "scope": "repo read:org user:email",
            "refresh_token": "ghr_def",
            "refresh_token_expires_in": 15724800,
            "expires_in": 28800
        }"#;

        let resp: TokenResponse = serde_json::from_str(json).expect("deserialize");
        assert_eq!(resp.access_token, "ghu_abc");
        assert_eq!(resp.refresh_token.as_deref(), Some("ghr_def"));
        assert_eq!(resp.expires_in, Some(28800));
    }

    #[test]
    fn test_github_user_deserialization() {
        let json = r#"{
            "login": "octocat",
            "id": 1,
            "avatar_url": "https://avatars.githubusercontent.com/u/1",
            "name": "The Octocat",
            "email": "octocat@github.com"
        }"#;

        let user: GitHubUser = serde_json::from_str(json).expect("deserialize");
        assert_eq!(user.login, "octocat");
        assert_eq!(user.id, 1);
        assert_eq!(user.name.as_deref(), Some("The Octocat"));
        assert_eq!(user.email.as_deref(), Some("octocat@github.com"));
    }
}
