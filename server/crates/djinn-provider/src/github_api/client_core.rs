use anyhow::{Result, anyhow};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use std::sync::Arc;

use crate::oauth::github_app::GitHubAppTokens;
use crate::repos::CredentialRepository;

use super::ActionsJob;

/// GitHub REST API v3 base URL.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// Maximum number of token-refresh retries on 401 responses.
const MAX_REFRESH_RETRIES: u32 = 1;

/// Initial back-off duration for rate-limit retries (seconds).
const BACKOFF_INITIAL_SECS: u64 = 1;

/// Maximum back-off duration for rate-limit retries (seconds).
const BACKOFF_MAX_SECS: u64 = 60;

#[derive(Clone)]
pub(crate) struct GitHubApiCore {
    http: Client,
    cred_repo: Arc<CredentialRepository>,
    base_url: String,
}

impl GitHubApiCore {
    pub(crate) fn new(cred_repo: Arc<CredentialRepository>, base_url: String) -> Self {
        let http = Client::builder()
            .user_agent("djinn-server/0.1 (+https://github.com/djinnos/server)")
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            cred_repo,
            base_url,
        }
    }

    pub(crate) fn http(&self) -> &Client {
        &self.http
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Load the cached GitHub OAuth App user access token.
    async fn load_user_token(&self) -> Result<GitHubAppTokens> {
        GitHubAppTokens::load_from_db(&self.cred_repo)
            .await
            .ok_or_else(|| anyhow!("No GitHub App tokens found — please authenticate first"))
    }

    /// Execute a request using the user OAuth token directly (per ADR-039).
    /// Retries once with a refreshed token on 401.
    pub(crate) async fn send_with_retry<F, Fut>(&self, build_request: F) -> Result<Response>
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

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ActionsJobsResponse {
    pub(crate) jobs: Vec<ActionsJob>,
}

/// Inspect rate-limit headers and sleep if the limit has been exhausted.
///
/// - If `X-RateLimit-Remaining` is `0`, sleep until `X-RateLimit-Reset`.
/// - If status is `429 Too Many Requests` without rate-limit headers,
///   apply exponential back-off starting at [`BACKOFF_INITIAL_SECS`].
pub(crate) async fn handle_rate_limit(resp: Response) -> Result<Response> {
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
