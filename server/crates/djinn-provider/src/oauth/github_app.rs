//! GitHub OAuth App — Device Code flow.
//!
//! Uses the device code flow (no client_secret required) for user authentication:
//!  1. Request a device code from GitHub.
//!  2. User visits the verification URL and enters the code.
//!  3. Poll GitHub until the user authorizes.
//!  4. Store the user access token + refresh token in the credential vault.
//!
//! Tokens are stored encrypted in the credentials DB table under
//! `__OAUTH_GITHUB_APP`.

use anyhow::{Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::repos::CredentialRepository;

// ─── Constants ────────────────────────────────────────────────────────────────

/// GitHub device-code endpoint.
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// GitHub OAuth access-token endpoint (used for polling and refresh).
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// GitHub OAuth App client ID (Djinn AI, owned by @djinnos).
pub const CLIENT_ID: &str = "Ov23liBIL080Vt6WJs69";

/// Default polling max attempts (60 × 5s = 5 minutes).
const MAX_POLL_ATTEMPTS: u32 = 60;

/// Default polling interval in seconds.
const DEFAULT_INTERVAL_SECS: u64 = 5;

/// Credential key for storing GitHub OAuth tokens.
pub const GITHUB_APP_OAUTH_DB_KEY: &str = "__OAUTH_GITHUB_APP";

// ─── Token types ─────────────────────────────────────────────────────────────

/// Cached GitHub OAuth App token bundle.
///
/// OAuth App tokens obtained via device code flow are long-lived and do not
/// expire or require refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubAppTokens {
    /// User access token for authenticating as the user.
    pub access_token: String,
    /// GitHub user login extracted after auth (best-effort).
    pub user_login: Option<String>,
    // Legacy fields kept for backward-compatible deserialization of existing
    // DB entries. Ignored at runtime.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    expires_at: Option<i64>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    refresh_token_expires_at: Option<i64>,
}

impl GitHubAppTokens {
    /// OAuth App tokens never expire.
    pub fn is_expired(&self) -> bool {
        false
    }

    /// Load tokens from the encrypted credential DB.
    pub async fn load_from_db(repo: &CredentialRepository) -> Option<Self> {
        if let Ok(Some(json)) = repo.get_decrypted(GITHUB_APP_OAUTH_DB_KEY).await {
            if let Ok(tokens) = serde_json::from_str::<Self>(&json) {
                return Some(tokens);
            }
            tracing::warn!("GitHubApp: corrupt token JSON in DB, ignoring");
        }
        None
    }

    /// Persist tokens to the encrypted credential DB.
    pub async fn save_to_db(&self, repo: &CredentialRepository) -> Result<()> {
        let json = serde_json::to_string(self)?;
        repo.set("github_app", GITHUB_APP_OAUTH_DB_KEY, &json)
            .await
            .map_err(|e| anyhow!("failed to save GitHub App tokens to DB: {e}"))?;
        Ok(())
    }

    /// Remove tokens from the DB.
    pub async fn clear_from_db(repo: &CredentialRepository) {
        let _ = repo.delete(GITHUB_APP_OAUTH_DB_KEY).await;
    }
}

// ─── Token response parsing ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

fn token_response_to_tokens(tr: TokenResponse, user_login: Option<String>) -> GitHubAppTokens {
    GitHubAppTokens {
        access_token: tr.access_token,
        user_login,
        refresh_token: None,
        expires_at: None,
        refresh_token_expires_at: None,
    }
}

// ─── Device code flow ────────────────────────────────────────────────────────

/// Data returned from the first phase of the device code flow.
#[derive(Debug)]
pub struct DeviceCodeSession {
    /// Short code the user types at `verification_uri`.
    pub user_code: String,
    /// URL the user visits to authorize (e.g. `https://github.com/login/device`).
    pub verification_uri: String,
    /// Pre-filled verification URL (includes user code), if provided by GitHub.
    pub verification_uri_complete: Option<String>,
    /// Opaque device code used when polling (not shown to user).
    device_code: String,
    /// Recommended polling interval in seconds.
    interval: u64,
}

/// Request a device code from GitHub.
pub async fn start_device_flow() -> Result<DeviceCodeSession> {
    let client = Client::new();

    #[derive(Serialize)]
    struct DeviceCodeRequest {
        client_id: &'static str,
        /// OAuth Apps require explicit scopes (GitHub Apps derive them from
        /// the app's configured permissions, but OAuth Apps do not).
        scope: &'static str,
    }

    #[derive(Deserialize)]
    struct DeviceCodeResponse {
        device_code: String,
        user_code: String,
        verification_uri: String,
        verification_uri_complete: Option<String>,
        interval: Option<u64>,
    }

    let resp = client
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .json(&DeviceCodeRequest {
            client_id: CLIENT_ID,
            scope: "repo",
        })
        .send()
        .await
        .map_err(|e| anyhow!("device code request failed: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("device code endpoint error ({}): {}", status, body));
    }

    let resp: DeviceCodeResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to parse device code response: {}", e))?;

    Ok(DeviceCodeSession {
        user_code: resp.user_code,
        verification_uri: resp.verification_uri,
        verification_uri_complete: resp.verification_uri_complete,
        device_code: resp.device_code,
        interval: resp.interval.unwrap_or(DEFAULT_INTERVAL_SECS),
    })
}

/// Poll GitHub until the user authorizes, then store tokens. Public so the MCP
/// layer can spawn this as a background task after returning the device code.
pub async fn poll_and_store(
    session: &DeviceCodeSession,
    repo: &CredentialRepository,
) -> Result<GitHubAppTokens> {
    let tr = poll_device_flow(session).await?;
    let user_login = fetch_user_login(&tr.access_token).await;
    let tokens = token_response_to_tokens(tr, user_login);
    tokens.save_to_db(repo).await?;
    tracing::info!(user = ?tokens.user_login, "GitHubApp: authentication successful");

    Ok(tokens)
}

/// Poll GitHub until the user authorizes, then return the token response.
async fn poll_device_flow(session: &DeviceCodeSession) -> Result<TokenResponse> {
    let client = Client::new();

    for attempt in 0..MAX_POLL_ATTEMPTS {
        tokio::time::sleep(std::time::Duration::from_secs(session.interval)).await;

        #[derive(Serialize)]
        struct PollRequest<'a> {
            client_id: &'static str,
            device_code: &'a str,
            grant_type: &'static str,
        }

        let body: serde_json::Value = client
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&PollRequest {
                client_id: CLIENT_ID,
                device_code: &session.device_code,
                grant_type: "urn:ietf:params:oauth:grant-type:device_code",
            })
            .send()
            .await
            .map_err(|e| anyhow!("poll request failed: {}", e))?
            .json()
            .await
            .map_err(|e| anyhow!("failed to parse poll response: {}", e))?;

        if let Some(error) = body["error"].as_str() {
            match error {
                "authorization_pending" => {
                    tracing::debug!(
                        "GitHubApp: authorization pending (attempt {}/{})",
                        attempt + 1,
                        MAX_POLL_ATTEMPTS
                    );
                    continue;
                }
                "slow_down" => {
                    tracing::debug!("GitHubApp: slow_down received, adding extra delay");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                "expired_token" => {
                    return Err(anyhow!("device code expired; please restart the flow"));
                }
                other => {
                    let desc = body["error_description"].as_str().unwrap_or("");
                    return Err(anyhow!("GitHub OAuth error: {} — {}", other, desc));
                }
            }
        }

        // Parse the successful token response from the JSON value.
        let tr: TokenResponse = serde_json::from_value(body.clone()).map_err(|e| {
            anyhow!(
                "GitHubApp: failed to decode token response ({}): {}",
                e,
                body
            )
        })?;
        return Ok(tr);
    }

    Err(anyhow!(
        "GitHubApp device code flow timed out after {} attempts",
        MAX_POLL_ATTEMPTS
    ))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", url])
        .spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    tracing::info!("Please open this URL in your browser: {}", url);
}

/// Fetch the authenticated user's login from the GitHub API.
async fn fetch_user_login(access_token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct GhUser {
        login: String,
    }
    let client = Client::new();
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .header("User-Agent", "djinn-server")
        .send()
        .await
        .ok()?;
    let user: GhUser = resp.json().await.ok()?;
    Some(user.login)
}

// ─── Full device code flow ───────────────────────────────────────────────────

/// Perform the full GitHub OAuth device code flow.
///
/// 1. Returns cached token if available (OAuth App tokens don't expire).
/// 2. Otherwise runs the device code flow.
///
/// Tokens are persisted to the encrypted credential DB on success.
pub async fn run_github_app_flow(repo: &CredentialRepository) -> Result<GitHubAppTokens> {
    // Return cached token — OAuth App tokens are long-lived.
    if let Some(cached) = GitHubAppTokens::load_from_db(repo).await {
        tracing::debug!("GitHubApp: using cached access token");
        return Ok(cached);
    }

    // 2. Device code flow
    let session = start_device_flow().await?;

    tracing::info!(
        user_code = %session.user_code,
        verification_uri = %session.verification_uri,
        "GitHubApp: enter code at verification URL"
    );

    // Open the browser — prefer the pre-filled URL if GitHub provided one
    let browser_url = session
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&session.verification_uri);
    open_browser(browser_url);

    // 3. Poll until user authorizes
    let tr = poll_device_flow(&session).await?;
    let user_login = fetch_user_login(&tr.access_token).await;
    let tokens = token_response_to_tokens(tr, user_login);
    tokens.save_to_db(repo).await?;
    tracing::info!(user = ?tokens.user_login, "GitHubApp: authentication successful");

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_app_tokens_never_expire() {
        let tokens = GitHubAppTokens {
            access_token: "tok".into(),
            user_login: None,
            refresh_token: None,
            expires_at: None,
            refresh_token_expires_at: None,
        };
        assert!(!tokens.is_expired());
    }

    #[test]
    fn token_response_to_tokens_sets_user_login() {
        let tr = TokenResponse {
            access_token: "at".into(),
        };
        let tokens = token_response_to_tokens(tr, Some("djinn-user".into()));
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.user_login.as_deref(), Some("djinn-user"));
    }

    #[test]
    fn legacy_db_format_deserializes() {
        // Old tokens stored with refresh_token and expires_at should still load.
        let json =
            r#"{"access_token":"tok","refresh_token":"rt","expires_at":1000,"user_login":"u"}"#;
        let tokens: GitHubAppTokens = serde_json::from_str(json).unwrap();
        assert_eq!(tokens.access_token, "tok");
        assert!(!tokens.is_expired());
    }
}
