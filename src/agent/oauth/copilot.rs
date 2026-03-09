//! GitHub Copilot OAuth — Device Code flow.
//!
//! Ported from Goose's `githubcopilot.rs`. Implements the two-phase flow:
//!  1. `start_copilot_flow()` — request a device code, return it for display.
//!  2. `poll_copilot_flow()` — poll until the user authorizes, then exchange
//!     the GitHub token for a short-lived Copilot API token.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── Constants ────────────────────────────────────────────────────────────────

/// OAuth client ID used by the VS Code Copilot extension (same as Goose).
const GITHUB_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// GitHub device-code endpoint.
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// GitHub OAuth access-token polling endpoint.
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Copilot internal token exchange endpoint.
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Default polling max attempts (36 × 5s = 3 minutes).
const MAX_POLL_ATTEMPTS: u32 = 36;

/// Default polling interval in seconds.
const DEFAULT_INTERVAL_SECS: u64 = 5;

/// Default Copilot model to use.
pub const COPILOT_DEFAULT_MODEL: &str = "gpt-4.1";

// ─── Token types ─────────────────────────────────────────────────────────────

/// Cached Copilot tokens (both GitHub access token and Copilot API token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotTokens {
    /// GitHub personal access token obtained via device code flow.
    pub github_token: String,
    /// Short-lived Copilot API token (refreshed from github_token).
    pub copilot_token: String,
    /// Copilot API endpoint from the token exchange response.
    pub api_endpoint: String,
    /// Unix timestamp when `copilot_token` expires (if known).
    pub expires_at: Option<i64>,
}

impl CopilotTokens {
    /// Returns true if the Copilot token has expired (with a 60-second buffer).
    pub fn is_expired(&self) -> bool {
        let Some(exp) = self.expires_at else {
            return false; // unknown — assume valid
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        now >= exp - 60
    }

    /// Disk cache path.
    pub fn cache_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".djinn")
            .join("oauth")
            .join("copilot.json")
    }

    /// Load from cache.
    pub fn load_cached() -> Option<Self> {
        let content = std::fs::read_to_string(Self::cache_path()).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save to cache.
    pub fn save(&self) -> Result<()> {
        let path = Self::cache_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Remove the cached token file.
    pub fn clear() {
        let _ = std::fs::remove_file(Self::cache_path());
    }
}

// ─── Device code session ──────────────────────────────────────────────────────

/// Data returned from the first phase of the device code flow.
///
/// Callers should display `user_code` and `verification_uri` to the user, then
/// call `poll_copilot_flow(session)` to await completion.
#[derive(Debug)]
pub struct DeviceCodeSession {
    /// Short code the user types at `verification_uri`.
    pub user_code: String,
    /// URL the user visits to authorize (e.g. `https://github.com/login/device`).
    pub verification_uri: String,
    /// Opaque device code used when polling (not shown to user).
    device_code: String,
    /// Recommended polling interval in seconds.
    interval: u64,
}

// ─── HTTP client ──────────────────────────────────────────────────────────────

fn build_client() -> Result<Client> {
    // Spoof VS Code user-agent — required by GitHub Copilot API.
    reqwest::ClientBuilder::new()
        .user_agent("GithubCopilot/1.155.0")
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::ACCEPT,
                "application/json".parse().unwrap(),
            );
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                "application/json".parse().unwrap(),
            );
            headers.insert("editor-version", "vscode/1.85.1".parse().unwrap());
            headers.insert("editor-plugin-version", "copilot/1.155.0".parse().unwrap());
            headers
        })
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {}", e))
}

// ─── Phase 1: request device code ─────────────────────────────────────────────

/// Request a device code from GitHub and return session info for display.
pub async fn start_copilot_flow() -> Result<DeviceCodeSession> {
    let client = build_client()?;

    #[derive(Serialize)]
    struct DeviceCodeRequest {
        client_id: &'static str,
        scope: &'static str,
    }

    #[derive(Deserialize)]
    struct DeviceCodeResponse {
        device_code: String,
        user_code: String,
        verification_uri: String,
        interval: Option<u64>,
    }

    let resp: DeviceCodeResponse = client
        .post(DEVICE_CODE_URL)
        .json(&DeviceCodeRequest {
            client_id: GITHUB_COPILOT_CLIENT_ID,
            scope: "read:user",
        })
        .send()
        .await
        .map_err(|e| anyhow!("device code request failed: {}", e))?
        .error_for_status()
        .map_err(|e| anyhow!("device code endpoint error: {}", e))?
        .json()
        .await
        .map_err(|e| anyhow!("failed to parse device code response: {}", e))?;

    Ok(DeviceCodeSession {
        user_code: resp.user_code,
        verification_uri: resp.verification_uri,
        device_code: resp.device_code,
        interval: resp.interval.unwrap_or(DEFAULT_INTERVAL_SECS),
    })
}

// ─── Phase 2: poll + exchange for Copilot token ───────────────────────────────

/// Poll GitHub until the user authorizes, then exchange for a Copilot API token.
///
/// Returns `CopilotTokens` which includes both the GitHub token (for future
/// Copilot-token refreshes) and the short-lived Copilot API token.
pub async fn poll_copilot_flow(session: DeviceCodeSession) -> Result<CopilotTokens> {
    let client = build_client()?;

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
            .json(&PollRequest {
                client_id: GITHUB_COPILOT_CLIENT_ID,
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
                        "Copilot: authorization pending (attempt {}/{})",
                        attempt + 1,
                        MAX_POLL_ATTEMPTS
                    );
                    continue;
                }
                "slow_down" => {
                    tracing::debug!("Copilot: slow_down received, adding extra delay");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                "expired_token" => return Err(anyhow!("device code expired; please restart the flow")),
                other => return Err(anyhow!("OAuth error from GitHub: {}", other)),
            }
        }

        if let Some(github_token) = body["access_token"].as_str() {
            tracing::info!("Copilot: GitHub token obtained, exchanging for Copilot API token");
            let tokens = exchange_for_copilot_token(&client, github_token).await?;
            let _ = tokens.save();
            return Ok(tokens);
        }

        tracing::debug!("Copilot: unexpected poll response: {}", body);
    }

    Err(anyhow!(
        "Copilot device code flow timed out after {} attempts",
        MAX_POLL_ATTEMPTS
    ))
}

// ─── Copilot token exchange ───────────────────────────────────────────────────

/// Exchange a GitHub access token for a short-lived Copilot API token.
pub async fn exchange_for_copilot_token(client: &Client, github_token: &str) -> Result<CopilotTokens> {
    #[derive(Deserialize)]
    struct CopilotTokenResponse {
        token: String,
        expires_at: Option<i64>,
        endpoints: Option<CopilotEndpoints>,
    }

    #[derive(Deserialize)]
    struct CopilotEndpoints {
        api: Option<String>,
    }

    let resp: CopilotTokenResponse = client
        .get(COPILOT_TOKEN_URL)
        .header("Authorization", format!("bearer {}", github_token))
        .send()
        .await
        .map_err(|e| anyhow!("Copilot token request failed: {}", e))?
        .error_for_status()
        .map_err(|e| anyhow!("Copilot token endpoint error: {}", e))?
        .json()
        .await
        .map_err(|e| anyhow!("failed to parse Copilot token response: {}", e))?;

    let api_endpoint = resp
        .endpoints
        .as_ref()
        .and_then(|e| e.api.as_deref())
        .unwrap_or("https://api.githubcopilot.com")
        .to_owned();

    Ok(CopilotTokens {
        github_token: github_token.to_owned(),
        copilot_token: resp.token,
        api_endpoint,
        expires_at: resp.expires_at,
    })
}

/// Refresh the Copilot API token using a stored GitHub token.
///
/// Used to silently renew the short-lived Copilot token without re-running
/// the full device code flow.
pub async fn refresh_copilot_token(github_token: &str) -> Result<CopilotTokens> {
    let client = build_client()?;
    exchange_for_copilot_token(&client, github_token).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_token_when_past_expiry() {
        let tokens = CopilotTokens {
            github_token: "gh_tok".into(),
            copilot_token: "cop_tok".into(),
            api_endpoint: "https://api.githubcopilot.com".into(),
            expires_at: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    - 100,
            ),
        };
        assert!(tokens.is_expired());
    }

    #[test]
    fn valid_token_when_far_future() {
        let tokens = CopilotTokens {
            github_token: "gh_tok".into(),
            copilot_token: "cop_tok".into(),
            api_endpoint: "https://api.githubcopilot.com".into(),
            expires_at: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    + 3600,
            ),
        };
        assert!(!tokens.is_expired());
    }

    #[test]
    fn unknown_expiry_treated_as_valid() {
        let tokens = CopilotTokens {
            github_token: "gh_tok".into(),
            copilot_token: "cop_tok".into(),
            api_endpoint: "https://api.githubcopilot.com".into(),
            expires_at: None,
        };
        assert!(!tokens.is_expired());
    }
}
