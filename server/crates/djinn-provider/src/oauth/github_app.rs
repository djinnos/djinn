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

/// Cached GitHub OAuth token bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubAppTokens {
    /// User access token for authenticating as the user.
    pub access_token: String,
    /// Refresh token for renewing the access token.
    pub refresh_token: String,
    /// UTC timestamp when the access token expires.
    pub expires_at: i64,
    /// UTC timestamp when the refresh token expires (optional).
    pub refresh_token_expires_at: Option<i64>,
    /// GitHub user login extracted after auth (best-effort).
    pub user_login: Option<String>,
}

impl GitHubAppTokens {
    /// Returns true if the access token has expired (with a 60-second buffer).
    pub fn is_expired(&self) -> bool {
        now_secs() >= self.expires_at - 60
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
    refresh_token: String,
    expires_in: Option<i64>,
    refresh_token_expires_in: Option<i64>,
}

fn token_response_to_tokens(tr: TokenResponse, user_login: Option<String>) -> GitHubAppTokens {
    let now = now_secs();
    let expires_at = now + tr.expires_in.unwrap_or(28_800); // default 8 hours
    let refresh_token_expires_at = tr.refresh_token_expires_in.map(|secs| now + secs);
    GitHubAppTokens {
        access_token: tr.access_token,
        refresh_token: tr.refresh_token,
        expires_at,
        refresh_token_expires_at,
        user_login,
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ─── Token refresh ───────────────────────────────────────────────────────────

/// Attempt a silent token refresh using the stored refresh_token.
/// Returns refreshed `GitHubAppTokens` on success, saving them to the DB.
pub async fn refresh_cached_token(
    cached: &GitHubAppTokens,
    client_id: &str,
    repo: &CredentialRepository,
) -> Result<GitHubAppTokens> {
    let client = Client::new();
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", cached.refresh_token.as_str()),
        ("client_id", client_id),
    ];
    let resp = client
        .post(ACCESS_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&params)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "GitHub App token refresh failed ({}): {}",
            status,
            text
        ));
    }
    let body = resp.text().await?;
    let tr: TokenResponse = serde_json::from_str(&body).map_err(|e| {
        anyhow!(
            "GitHub App token refresh: failed to decode response ({}): {}",
            e,
            body
        )
    })?;
    let tokens = token_response_to_tokens(tr, cached.user_login.clone());
    tokens.save_to_db(repo).await?;
    tracing::info!("GitHubApp: token refreshed successfully");
    Ok(tokens)
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
/// 1. Checks DB cache; returns immediately if unexpired.
/// 2. Attempts a silent token refresh if the cached token is expired.
/// 3. Falls back to a full device code flow if refresh fails or no cache exists.
///
/// Tokens are persisted to the encrypted credential DB on success.
pub async fn run_github_app_flow(repo: &CredentialRepository) -> Result<GitHubAppTokens> {
    // 1. Check cache
    if let Some(cached) = GitHubAppTokens::load_from_db(repo).await {
        if !cached.is_expired() {
            tracing::debug!("GitHubApp: using cached access token");
            return Ok(cached);
        }
        tracing::debug!("GitHubApp: cached token expired, attempting refresh");
        match refresh_cached_token(&cached, CLIENT_ID, repo).await {
            Ok(tokens) => return Ok(tokens),
            Err(e) => {
                tracing::warn!(
                    "GitHubApp: token refresh failed, starting device flow: {}",
                    e
                );
            }
        }
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
    fn expired_token_detection() {
        let tokens = GitHubAppTokens {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: now_secs() - 100,
            refresh_token_expires_at: None,
            user_login: None,
        };
        assert!(tokens.is_expired());

        let tokens_valid = GitHubAppTokens {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: now_secs() + 3600,
            refresh_token_expires_at: None,
            user_login: None,
        };
        assert!(!tokens_valid.is_expired());
    }

    #[test]
    fn token_response_sets_expires_at_from_expires_in() {
        let before = now_secs();
        let tr = TokenResponse {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_in: Some(3600),
            refresh_token_expires_in: Some(86400),
        };
        let tokens = token_response_to_tokens(tr, Some("djinn-user".into()));
        let after = now_secs();
        assert!(tokens.expires_at >= before + 3600);
        assert!(tokens.expires_at <= after + 3600);
        assert!(tokens.refresh_token_expires_at.is_some());
        assert_eq!(tokens.user_login.as_deref(), Some("djinn-user"));
    }
}
