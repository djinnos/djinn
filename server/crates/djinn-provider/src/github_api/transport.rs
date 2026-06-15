use anyhow::{Result, anyhow};
use reqwest::{Response, StatusCode};
use thiserror::Error;

use crate::github_api::{AuthMode, GitHubApiClient, GitHubApiError};

/// Returned by [`GitHubApiClient::send_with_retry`] when a `UserToken`
/// auth mode receives a 401. Callers can downcast via
/// `err.downcast_ref::<UserTokenExpired>()` to skip the action gracefully
/// (e.g. pr_poller falls back to "wait for human approval" instead of
/// failing the run) without parsing error message strings.
#[derive(Debug, Error)]
#[error("GitHub user token expired or revoked")]
pub struct UserTokenExpired;

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
            AuthMode::SessionUser => djinn_core::auth_context::current_user_token()
                .ok_or_else(|| anyhow!("sign in with GitHub required")),
            AuthMode::Installation { installation_id } => {
                let tok = crate::github_app::get_installation_token(*installation_id)
                    .await
                    .map_err(|e| {
                        anyhow!("failed to mint installation token for {installation_id}: {e}")
                    })?;
                Ok(tok.token)
            }
            AuthMode::UserToken { current, .. } => Ok(current.read().await.clone()),
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
    /// tokens, consults the attached
    /// [`crate::github_api::UserTokenRefresh`] before giving up.
    pub(super) async fn send_with_retry<F, Fut>(
        &self,
        build_request: F,
    ) -> std::result::Result<Response, GitHubApiError>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<Response>>,
    {
        let token = self.bearer_token().await.map_err(|e| {
            GitHubApiError::transport("send_with_retry", "<auth>".to_string(), e.to_string())
        })?;
        let resp = build_request(token)
            .await
            .map_err(classify_transport_error)?;

        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }

        match &self.auth {
            AuthMode::SessionUser => Err(GitHubApiError::unauthenticated(
                "send_with_retry",
                "<request>".to_string(),
                "GitHub API returned 401 — token may have been revoked, please re-authenticate"
                    .to_string(),
            )),
            AuthMode::Installation { installation_id } => {
                tracing::warn!(
                    installation_id = *installation_id,
                    "github-api: 401 — refreshing installation token and retrying"
                );
                self.invalidate_cached_token();
                let token = self.bearer_token().await.map_err(|e| {
                    GitHubApiError::transport(
                        "send_with_retry",
                        "<auth>".to_string(),
                        e.to_string(),
                    )
                })?;
                build_request(token).await.map_err(classify_transport_error)
            }
            AuthMode::UserToken { current, refresher } => {
                tracing::warn!("github-api: 401 on user token — attempting refresh");
                match refresher.refresh().await {
                    Ok(new_token) => {
                        // Overwrite the cell so subsequent calls on this
                        // client (and any clones sharing the Arc) pick up
                        // the rotated token without another DB round-trip.
                        *current.write().await = new_token.clone();
                        build_request(new_token)
                            .await
                            .map_err(classify_transport_error)
                    }
                    Err(e) => {
                        tracing::info!(
                            error = %e,
                            "github-api: user-token refresh failed → surfacing UserTokenExpired"
                        );
                        Err(GitHubApiError::unauthenticated(
                            "send_with_retry",
                            "<request>".to_string(),
                            UserTokenExpired.to_string(),
                        ))
                    }
                }
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
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::Error::new(GitHubApiError::rate_limited(
            "handle_rate_limit",
            "<request>".to_string(),
            if body.is_empty() {
                format!("GitHub rate limit exhausted — retry after {sleep_secs}s")
            } else {
                body
            },
        )));
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
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow::Error::new(GitHubApiError::transport(
                    "handle_rate_limit",
                    "<request>".to_string(),
                    if body.is_empty() {
                        format!("GitHub API returned 429 after {attempts} retries")
                    } else {
                        body
                    },
                )));
            }
            delay = (delay * 2).min(BACKOFF_MAX_SECS);
        }
    }

    Ok(resp)
}

fn classify_transport_error(err: anyhow::Error) -> GitHubApiError {
    match err.downcast::<GitHubApiError>() {
        Ok(typed) => typed,
        Err(err) => {
            GitHubApiError::transport("send_with_retry", "<request>".to_string(), err.to_string())
        }
    }
}
