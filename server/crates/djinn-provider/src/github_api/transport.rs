use anyhow::{Result, anyhow};
use reqwest::{Response, StatusCode};

use crate::github_api::GitHubApiClient;
use crate::oauth::github_app::GitHubAppTokens;

/// Maximum number of token-refresh retries on 401 responses.
const MAX_REFRESH_RETRIES: u32 = 1;

/// Initial back-off duration for rate-limit retries (seconds).
const BACKOFF_INITIAL_SECS: u64 = 1;

/// Maximum back-off duration for rate-limit retries (seconds).
const BACKOFF_MAX_SECS: u64 = 60;

impl GitHubApiClient {
    /// Load the cached GitHub OAuth App user access token.
    pub(super) async fn load_user_token(&self) -> Result<GitHubAppTokens> {
        GitHubAppTokens::load_from_db(&self.cred_repo)
            .await
            .ok_or_else(|| anyhow!("No GitHub App tokens found — please authenticate first"))
    }

    /// Execute a request using the user OAuth token directly (per ADR-039).
    /// Returns an authentication error on 401.
    pub(super) async fn send_with_retry<F, Fut>(&self, build_request: F) -> Result<Response>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<Response>>,
    {
        let user_tokens = self.load_user_token().await?;
        let resp = build_request(user_tokens.access_token.clone()).await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "GitHub API returned 401 — token may have been revoked, please re-authenticate"
            ));
        }

        Ok(resp)
    }
}

/// Inspect rate-limit headers and sleep if the limit has been exhausted.
///
/// - If `X-RateLimit-Remaining` is `0`, sleep until `X-RateLimit-Reset`.
/// - If status is `429 Too Many Requests` without rate-limit headers,
///   apply exponential back-off starting at [`BACKOFF_INITIAL_SECS`].
pub(super) async fn handle_rate_limit(resp: Response) -> Result<Response> {
    let status = resp.status();
    let remaining = resp
        .headers()
        .get("X-RateLimit-Remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let reset = resp
        .headers()
        .get("X-RateLimit-Reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    if remaining == Some(0) {
        let sleep_secs = if let Some(reset_epoch) = reset {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            reset_epoch.saturating_sub(now).max(1)
        } else {
            BACKOFF_INITIAL_SECS
        };
        tracing::warn!(
            "GitHubApiClient: rate limit exhausted, sleeping {}s",
            sleep_secs
        );
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
        return Err(anyhow!(
            "GitHub rate limit exhausted — retry after {}s",
            sleep_secs
        ));
    }

    if status == StatusCode::TOO_MANY_REQUESTS && remaining.is_none() {
        let mut delay = BACKOFF_INITIAL_SECS;
        let mut attempts = 0u32;
        loop {
            tracing::warn!(
                "GitHubApiClient: 429 without rate-limit header, back-off {}s (attempt {})",
                delay,
                attempts + 1
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            attempts += 1;
            if attempts >= MAX_REFRESH_RETRIES || delay >= BACKOFF_MAX_SECS {
                return Err(anyhow!(
                    "GitHub API returned 429 after {} retries",
                    attempts
                ));
            }
            delay = (delay * 2).min(BACKOFF_MAX_SECS);
        }
    }

    Ok(resp)
}
