use anyhow::{Result, anyhow};
use reqwest::{Response, StatusCode};

use crate::github_api::{AuthMode, GitHubApiClient};

/// Maximum number of token-refresh retries on 401 responses.
const MAX_REFRESH_RETRIES: u32 = 1;

/// Initial back-off duration for rate-limit retries (seconds).
const BACKOFF_INITIAL_SECS: u64 = 1;

/// Maximum back-off duration for rate-limit retries (seconds).
const BACKOFF_MAX_SECS: u64 = 60;

impl GitHubApiClient {
    /// Resolve a bearer token for the next outbound request based on the
    /// configured [`AuthMode`].
    pub(super) async fn bearer_token(&self) -> Result<String> {
        match &self.auth {
            AuthMode::UserToken { cred_repo } => {
                let tokens = crate::github_app::user_token_compat::load_user_tokens(cred_repo)
                    .await
                    .ok_or_else(|| {
                        anyhow!("No GitHub App user token found — please authenticate first")
                    })?;
                Ok(tokens.access_token)
            }
            AuthMode::Installation { installation_id } => {
                let tok = crate::github_app::get_installation_token(*installation_id)
                    .await
                    .map_err(|e| {
                        anyhow!("failed to mint installation token for {installation_id}: {e}")
                    })?;
                Ok(tok.token)
            }
        }
    }

    /// Invalidate any cached bearer token. For installation-scoped clients
    /// this drops the cached installation token; for user-token clients
    /// there is nothing to invalidate (the row is reloaded on the next
    /// call).
    fn invalidate_cached_token(&self) {
        if let AuthMode::Installation { installation_id } = &self.auth {
            crate::github_app::installations::invalidate_cache(*installation_id);
        }
    }

    /// Execute a request using the configured auth mode. Retries once with
    /// a refreshed token on 401 for installation-scoped clients; for user
    /// tokens a 401 surfaces a re-auth error immediately.
    pub(super) async fn send_with_retry<F, Fut>(&self, build_request: F) -> Result<Response>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<Response>>,
    {
        let token = self.bearer_token().await?;
        let resp = build_request(token).await?;

        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        match &self.auth {
            AuthMode::UserToken { .. } => Err(anyhow!(
                "GitHub API returned 401 — token may have been revoked, please re-authenticate"
            )),
            AuthMode::Installation { installation_id } => {
                tracing::warn!(
                    installation_id = *installation_id,
                    "github-api: 401 — refreshing installation token and retrying"
                );
                self.invalidate_cached_token();
                let token = self.bearer_token().await?;
                build_request(token).await
            }
        }
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
