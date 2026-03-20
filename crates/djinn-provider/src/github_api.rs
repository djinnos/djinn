//! GitHub REST API v3 client.
//!
//! Provides a [`GitHubApiClient`] that derives installation access tokens from
//! the user OAuth token + installation ID stored in the credential vault.
//!
//! # Operations
//! - [`GitHubApiClient::derive_installation_token`] — exchange user token for installation token
//! - [`GitHubApiClient::create_pull_request`] — open a PR
//! - [`GitHubApiClient::enable_auto_merge`] — set auto-merge on a PR
//! - [`GitHubApiClient::get_pull_request`] — fetch PR status and CI checks
//! - [`GitHubApiClient::list_pull_request_reviews`] — list inline review comments
//! - [`GitHubApiClient::list_pr_review_states`] — list top-level review states (APPROVED, CHANGES_REQUESTED, etc.)
//! - [`GitHubApiClient::fetch_pr_review_feedback`] — aggregate CHANGES_REQUESTED reviews + inline comments into [`PrReviewFeedback`]
//! - [`GitHubApiClient::re_request_review`] — re-request review from previous reviewers after fixup commits
//!
//! # Token lifecycle
//! On every API call the client checks whether the cached user token is
//! expired and refreshes it via `refresh_cached_token()` when needed.
//! On a `401 Unauthorized` response from the GitHub API, the client
//! performs one automatic token refresh and retries the request.
//!
//! # Rate limiting
//! Responses that carry `X-RateLimit-Remaining: 0` cause the client to
//! sleep until `X-RateLimit-Reset` (epoch seconds) before returning the
//! response.  If the header is absent the client falls back to exponential
//! back-off on `429 Too Many Requests`.

use anyhow::{Result, anyhow};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::oauth::github_app::{
    GITHUB_APP_CLIENT_ID_KEY, GITHUB_INSTALLATION_ID_KEY,
    GitHubAppTokens, refresh_cached_token,
};
use crate::repos::CredentialRepository;

// ─── Constants ────────────────────────────────────────────────────────────────

/// GitHub REST API v3 base URL.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// Maximum number of token-refresh retries on 401 responses.
const MAX_REFRESH_RETRIES: u32 = 1;

/// Initial back-off duration for rate-limit retries (seconds).
const BACKOFF_INITIAL_SECS: u64 = 1;

/// Maximum back-off duration for rate-limit retries (seconds).
const BACKOFF_MAX_SECS: u64 = 60;

// ─── Installation token ───────────────────────────────────────────────────────

/// Short-lived installation access token returned by GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationToken {
    /// Bearer token for making GitHub API calls as the installation.
    pub token: String,
    /// ISO-8601 expiry timestamp.
    pub expires_at: String,
}

// ─── PR types ─────────────────────────────────────────────────────────────────

/// Parameters for creating a pull request.
#[derive(Debug, Clone, Serialize)]
pub struct CreatePrParams {
    pub title: String,
    pub body: String,
    /// Name of the branch to merge.
    pub head: String,
    /// Target branch.
    pub base: String,
    /// Whether to allow maintainers to push to the branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintainer_can_modify: Option<bool>,
    /// Whether the PR should be created as a draft.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
}

/// Merge method for auto-merge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    #[default]
    Merge,
    Squash,
    Rebase,
}

/// Pull request state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Open,
    Closed,
}

/// A single CI check run associated with a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: String,
}

/// Summary of check runs for a PR head SHA.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRunsResponse {
    pub total_count: u32,
    pub check_runs: Vec<CheckRun>,
}

/// A pull request returned by the GitHub API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: PrState,
    pub merged: Option<bool>,
    pub html_url: String,
    pub head: PrRef,
    pub base: PrRef,
    pub auto_merge: Option<serde_json::Value>,
    pub node_id: String,
}

/// A branch/commit reference embedded in a PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

/// A top-level PR review (submitted via "Review changes" — distinct from inline comments).
///
/// The `state` field is one of `"APPROVED"`, `"CHANGES_REQUESTED"`, `"COMMENTED"`,
/// `"DISMISSED"`, or `"PENDING"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrReview {
    pub id: u64,
    pub user: Option<GitHubUser>,
    pub state: String,
    pub submitted_at: Option<String>,
    pub html_url: String,
    /// The general review body (non-line-specific).  May be empty or absent.
    #[serde(default)]
    pub body: String,
}

/// A review comment on a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub user: Option<GitHubUser>,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    pub pull_request_review_id: Option<u64>,
    pub path: Option<String>,
    pub line: Option<u32>,
}

/// Minimal GitHub user object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    pub id: u64,
}

/// Aggregated PR review feedback used to prime worker sessions during the
/// review-feedback dispatch loop (ADR-037 Phase 4).
///
/// Combines top-level review states (CHANGES_REQUESTED) and inline review
/// comments into a single structured payload that is stored as a
/// `pr_review_feedback` activity log entry and surfaced in the worker prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrReviewFeedback {
    /// PR number on GitHub.
    pub pull_number: u64,
    /// GitHub PR URL.
    pub pr_url: String,
    /// Top-level reviews that have `CHANGES_REQUESTED` state.
    pub change_request_reviews: Vec<PrReview>,
    /// Inline code comments from all reviewers.
    pub inline_comments: Vec<ReviewComment>,
}

// ─── Client ───────────────────────────────────────────────────────────────────

/// GitHub REST API v3 client.
///
/// Holds a reference to the credential repository for loading and refreshing
/// OAuth tokens, and an optional override for the API base URL (used in tests).
#[derive(Clone)]
pub struct GitHubApiClient {
    http: Client,
    cred_repo: Arc<CredentialRepository>,
    /// Override for the GitHub API base URL (default: `GITHUB_API_BASE`).
    base_url: String,
    /// Cached installation token — refreshed on expiry or 401.
    installation_token: Arc<Mutex<Option<InstallationToken>>>,
}

impl GitHubApiClient {
    /// Create a new client backed by `cred_repo`.
    pub fn new(cred_repo: Arc<CredentialRepository>) -> Self {
        Self::with_base_url(cred_repo, GITHUB_API_BASE.to_string())
    }

    /// Create a new client with a custom API base URL (useful for tests).
    pub fn with_base_url(cred_repo: Arc<CredentialRepository>, base_url: String) -> Self {
        let http = Client::builder()
            .user_agent("djinn-server/0.1 (+https://github.com/djinnos/server)")
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            cred_repo,
            base_url,
            installation_token: Arc::new(Mutex::new(None)),
        }
    }

    // ─── Token management ─────────────────────────────────────────────────────

    /// Load and (if needed) refresh the GitHub App user access token.
    async fn load_user_token(&self) -> Result<GitHubAppTokens> {
        let tokens = GitHubAppTokens::load_from_db(&self.cred_repo)
            .await
            .ok_or_else(|| anyhow!("No GitHub App tokens found — please authenticate first"))?;

        if tokens.is_expired() {
            let client_id = self
                .cred_repo
                .get_decrypted(GITHUB_APP_CLIENT_ID_KEY)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "Iv23liGitHubAppDjinn".to_string());
            tracing::debug!("GitHubApiClient: user token expired, refreshing");
            return refresh_cached_token(&tokens, &client_id, &self.cred_repo).await;
        }
        Ok(tokens)
    }

    /// Derive (or return a cached) installation access token.
    ///
    /// Calls `POST /app/installations/{installation_id}/access_tokens` using the
    /// user bearer token. Caches the result until the token is about to expire.
    pub async fn derive_installation_token(&self) -> Result<InstallationToken> {
        // Return cached token if still valid.
        {
            let guard = self.installation_token.lock().await;
            if let Some(ref cached) = *guard
                && !installation_token_expired(cached)
            {
                return Ok(cached.clone());
            }
        }

        let user_tokens = self.load_user_token().await?;
        let installation_id = self
            .cred_repo
            .get_decrypted(GITHUB_INSTALLATION_ID_KEY)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| anyhow!("No GitHub installation ID found — call store_installation_id first"))?;

        let url = format!(
            "{}/app/installations/{}/access_tokens",
            self.base_url, installation_id
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&user_tokens.access_token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;

        let resp = handle_rate_limit(resp).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Failed to derive installation token ({}): {}",
                status,
                body
            ));
        }

        let token: InstallationToken = resp.json().await?;
        *self.installation_token.lock().await = Some(token.clone());
        Ok(token)
    }

    // ─── Core request helper ──────────────────────────────────────────────────

    /// Execute a request, retrying once with a fresh token on 401.
    async fn send_with_retry<F, Fut>(&self, build_request: F) -> Result<Response>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Result<Response>>,
    {
        let token = self.derive_installation_token().await?;
        let resp = build_request(token.token.clone()).await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            tracing::warn!("GitHubApiClient: 401 received, invalidating installation token and retrying");
            // Invalidate cached installation token so next call re-derives it.
            *self.installation_token.lock().await = None;

            // Also check if the user token needs refresh.
            let user_tokens = GitHubAppTokens::load_from_db(&self.cred_repo)
                .await
                .ok_or_else(|| anyhow!("No GitHub App tokens found for refresh"))?;
            let client_id = self
                .cred_repo
                .get_decrypted(GITHUB_APP_CLIENT_ID_KEY)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "Iv23liGitHubAppDjinn".to_string());
            refresh_cached_token(&user_tokens, &client_id, &self.cred_repo).await?;

            // Re-derive installation token and retry once.
            let fresh_token = self.derive_installation_token().await?;
            return build_request(fresh_token.token).await;
        }

        Ok(resp)
    }

    // ─── PR operations ────────────────────────────────────────────────────────

    /// Create a pull request.
    ///
    /// `owner` and `repo` identify the repository. Returns the created PR.
    pub async fn create_pull_request(
        &self,
        owner: &str,
        repo: &str,
        params: CreatePrParams,
    ) -> Result<PullRequest> {
        let url = format!("{}/repos/{}/{}/pulls", self.base_url, owner, repo);
        let body = serde_json::to_value(&params)?;

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .post(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("create_pull_request failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
    }

    /// Enable auto-merge on an existing pull request.
    ///
    /// Uses the `PUT /repos/{owner}/{repo}/pulls/{pull_number}/merge` endpoint
    /// with `merge_method`. The PR must be open and the repo must have auto-merge
    /// enabled in settings.
    pub async fn enable_auto_merge(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        method: MergeMethod,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/merge",
            self.base_url, owner, repo, pull_number
        );
        let body = serde_json::json!({ "merge_method": method });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .put(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("enable_auto_merge failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
    }

    /// Get a pull request along with its CI check runs.
    ///
    /// Returns `(PullRequest, CheckRunsResponse)` for the given PR number.
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<(PullRequest, CheckRunsResponse)> {
        // Fetch PR.
        let pr_url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.base_url, owner, repo, pull_number
        );

        let pr_resp = self
            .send_with_retry(|token| {
                let url = pr_url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !pr_resp.status().is_success() {
            let status = pr_resp.status();
            let body = pr_resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_pull_request failed ({}): {}", status, body));
        }
        let pr: PullRequest = pr_resp.json().await?;

        // Fetch check runs for the PR's head SHA.
        let checks_url = format!(
            "{}/repos/{}/{}/commits/{}/check-runs",
            self.base_url, owner, repo, pr.head.sha
        );

        let checks_resp = self
            .send_with_retry(|token| {
                let url = checks_url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        let checks: CheckRunsResponse = if checks_resp.status().is_success() {
            checks_resp.json().await?
        } else {
            tracing::warn!(
                "GitHubApiClient: check-runs fetch failed ({}), returning empty",
                checks_resp.status()
            );
            CheckRunsResponse {
                total_count: 0,
                check_runs: vec![],
            }
        };

        Ok((pr, checks))
    }

    /// List review comments on a pull request.
    pub async fn list_pull_request_reviews(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<ReviewComment>> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/comments",
            self.base_url, owner, repo, pull_number
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_pull_request_reviews failed ({}): {}",
                status,
                body
            ));
        }
        Ok(resp.json().await?)
    }

    /// List top-level reviews submitted on a pull request.
    ///
    /// Returns each review's state (`"APPROVED"`, `"CHANGES_REQUESTED"`, etc.).
    /// Uses `GET /repos/{owner}/{repo}/pulls/{number}/reviews`.
    pub async fn list_pr_review_states(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<Vec<PrReview>> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/reviews",
            self.base_url, owner, repo, pull_number
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "list_pr_review_states failed ({}): {}",
                status,
                body
            ));
        }
        Ok(resp.json().await?)
    }

    /// Fetch aggregated PR review feedback for a pull request.
    ///
    /// Combines top-level review states (filtering to `CHANGES_REQUESTED`) and
    /// all inline review comments into a [`PrReviewFeedback`] payload.
    pub async fn fetch_pr_review_feedback(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        pr_url: &str,
    ) -> Result<PrReviewFeedback> {
        let reviews = self.list_pr_review_states(owner, repo, pull_number).await?;
        let inline_comments = self
            .list_pull_request_reviews(owner, repo, pull_number)
            .await?;

        let change_request_reviews = reviews
            .into_iter()
            .filter(|r| r.state == "CHANGES_REQUESTED")
            .collect();

        Ok(PrReviewFeedback {
            pull_number,
            pr_url: pr_url.to_owned(),
            change_request_reviews,
            inline_comments,
        })
    }

    /// Re-request review on a pull request from all reviewers who previously
    /// submitted a `CHANGES_REQUESTED` review.
    ///
    /// Uses `POST /repos/{owner}/{repo}/pulls/{pull_number}/requested_reviewers`.
    /// Non-fatal: logs a warning if the API call fails.
    pub async fn re_request_review(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        reviewer_logins: &[String],
    ) -> Result<()> {
        if reviewer_logins.is_empty() {
            return Ok(());
        }

        let url = format!(
            "{}/repos/{}/{}/pulls/{}/requested_reviewers",
            self.base_url, owner, repo, pull_number
        );
        let body = serde_json::json!({ "reviewers": reviewer_logins });

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let body = body.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .post(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .json(&body)
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("re_request_review failed ({}): {}", status, body));
        }
        Ok(())
    }
}

// ─── Rate limiting helper ─────────────────────────────────────────────────────

/// Inspect rate-limit headers and sleep if the limit has been exhausted.
///
/// - If `X-RateLimit-Remaining` is `0`, sleep until `X-RateLimit-Reset`.
/// - If status is `429 Too Many Requests` without rate-limit headers,
///   apply exponential back-off starting at [`BACKOFF_INITIAL_SECS`].
async fn handle_rate_limit(resp: Response) -> Result<Response> {
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
        return Err(anyhow!("GitHub rate limit exhausted — retry after {}s", sleep_secs));
    }

    if status == StatusCode::TOO_MANY_REQUESTS && remaining.is_none() {
        // Back-off without reset header.
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
                return Err(anyhow!("GitHub API returned 429 after {} retries", attempts));
            }
            delay = (delay * 2).min(BACKOFF_MAX_SECS);
        }
    }

    Ok(resp)
}

/// Returns `true` if the installation token has expired (60-second buffer).
fn installation_token_expired(token: &InstallationToken) -> bool {
    // expires_at is ISO-8601, e.g. "2024-01-01T12:00:00Z"
    let Ok(dt) = chrono_parse_iso8601(&token.expires_at) else {
        return false; // can't parse — assume still valid
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now >= dt - 60
}

/// Minimal ISO-8601 UTC parser: `YYYY-MM-DDTHH:MM:SSZ` → epoch seconds.
fn chrono_parse_iso8601(s: &str) -> Result<i64> {
    // Format: "2024-01-01T12:00:00Z"
    let s = s.trim_end_matches('Z');
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return Err(anyhow!("invalid ISO-8601: {}", s));
    }
    let date_parts: Vec<u32> = parts[0]
        .split('-')
        .filter_map(|p| p.parse().ok())
        .collect();
    let time_parts: Vec<u32> = parts[1]
        .split(':')
        .filter_map(|p| p.parse().ok())
        .collect();
    if date_parts.len() != 3 || time_parts.len() != 3 {
        return Err(anyhow!("invalid ISO-8601 parts: {}", s));
    }
    let (y, mo, d) = (date_parts[0] as i64, date_parts[1] as i64, date_parts[2] as i64);
    let (h, mi, sec) = (time_parts[0] as i64, time_parts[1] as i64, time_parts[2] as i64);

    // Days since Unix epoch (1970-01-01).
    // Zeller-like calculation for simplicity.
    let days = days_since_epoch(y, mo, d);
    Ok(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// Compute days since 1970-01-01 for a given Gregorian date.
fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let y = if month <= 2 { year - 1 } else { year };
    let m = month;
    let d = day;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use wiremock::matchers::{header, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use djinn_core::events::EventBus;
    use djinn_db::Database;

    use crate::oauth::github_app::{
        GITHUB_APP_OAUTH_DB_KEY, GITHUB_INSTALLATION_ID_KEY, GitHubAppTokens,
    };
    use crate::repos::CredentialRepository;

    // ─── Test helpers ──────────────────────────────────────────────────────────

    fn make_repo() -> Arc<CredentialRepository> {
        let db = Database::open_in_memory().expect("in-memory db");
        Arc::new(CredentialRepository::new(db, EventBus::noop()))
    }

    async fn seed_tokens(repo: &CredentialRepository, access_token: &str) {
        let tokens = GitHubAppTokens {
            access_token: access_token.to_string(),
            refresh_token: "rt_test".to_string(),
            expires_at: now_secs() + 3600,
            refresh_token_expires_at: None,
            user_login: Some("djinn-test".to_string()),
        };
        let json = serde_json::to_string(&tokens).unwrap();
        repo.set("github_app", GITHUB_APP_OAUTH_DB_KEY, &json)
            .await
            .unwrap();
        repo.set("github_app", GITHUB_INSTALLATION_ID_KEY, "12345678")
            .await
            .unwrap();
    }

    fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn future_expires_at() -> String {
        // One hour from now, formatted as ISO-8601.
        let secs = now_secs() + 3600;
        let days = secs / 86400;
        let rem = secs % 86400;
        let h = rem / 3600;
        let m = (rem % 3600) / 60;
        let s = rem % 60;
        // Simple date reconstruction — good enough for test tokens.
        // Epoch day → Gregorian: reuse the algorithm in reverse.
        let (year, month, day) = epoch_days_to_ymd(days);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", year, month, day, h, m, s)
    }

    fn epoch_days_to_ymd(z: i64) -> (i64, i64, i64) {
        let z = z + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        (y, m, d)
    }

    // ─── Unit tests ────────────────────────────────────────────────────────────

    #[test]
    fn iso8601_parse_known_date() {
        // 2024-01-01T00:00:00Z should be 1704067200
        let result = chrono_parse_iso8601("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(result, 1_704_067_200);
    }

    #[test]
    fn iso8601_parse_roundtrip() {
        let ts = now_secs();
        let days = ts / 86400;
        let rem = ts % 86400;
        let h = rem / 3600;
        let m = (rem % 3600) / 60;
        let s = rem % 60;
        let (year, month, day) = epoch_days_to_ymd(days);
        let formatted = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", year, month, day, h, m, s);
        let parsed = chrono_parse_iso8601(&formatted).unwrap();
        // Allow ±1 second for rounding.
        assert!((parsed - ts).abs() <= 1, "round-trip failed: {} vs {}", parsed, ts);
    }

    #[test]
    fn installation_token_not_expired_for_future() {
        let token = InstallationToken {
            token: "ghs_test".into(),
            expires_at: future_expires_at(),
        };
        assert!(!installation_token_expired(&token));
    }

    #[test]
    fn installation_token_expired_for_past() {
        let token = InstallationToken {
            token: "ghs_test".into(),
            expires_at: "2020-01-01T00:00:00Z".into(),
        };
        assert!(installation_token_expired(&token));
    }

    // ─── Integration tests with wiremock ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn derive_installation_token_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user_token").await;

        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .and(header("Authorization", "Bearer ghu_user_token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_installation_token",
                "expires_at": future_expires_at()
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let token = client.derive_installation_token().await.unwrap();
        assert_eq!(token.token, "ghs_installation_token");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn derive_installation_token_cached() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user_token").await;

        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_cached",
                "expires_at": future_expires_at()
            })))
            .expect(1) // Only one call — second call should use cache.
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        client.derive_installation_token().await.unwrap();
        client.derive_installation_token().await.unwrap();
        // wiremock will panic on drop if the expectation is violated.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_pull_request_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        // Installation token endpoint.
        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_tok",
                "expires_at": future_expires_at()
            })))
            .mount(&server)
            .await;

        // PR creation endpoint.
        Mock::given(method("POST"))
            .and(path("/repos/djinnos/server/pulls"))
            .and(header("Authorization", "Bearer ghs_tok"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "number": 42,
                "title": "feat: add feature",
                "state": "open",
                "merged": false,
                "html_url": "https://github.com/djinnos/server/pull/42",
                "head": { "ref": "feature-branch", "sha": "abc123" },
                "base": { "ref": "main", "sha": "def456" },
                "auto_merge": null,
                "node_id": "PR_abc123"
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let pr = client
            .create_pull_request(
                "djinnos",
                "server",
                CreatePrParams {
                    title: "feat: add feature".into(),
                    body: "Description".into(),
                    head: "feature-branch".into(),
                    base: "main".into(),
                    maintainer_can_modify: None,
                    draft: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "feat: add feature");
        assert_eq!(pr.state, PrState::Open);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enable_auto_merge_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_tok",
                "expires_at": future_expires_at()
            })))
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/repos/djinnos/server/pulls/42/merge"))
            .and(header("Authorization", "Bearer ghs_tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "sha": "abc123",
                "merged": true,
                "message": "Pull Request successfully merged"
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let result = client
            .enable_auto_merge("djinnos", "server", 42, MergeMethod::Squash)
            .await
            .unwrap();

        assert_eq!(result["merged"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_pull_request_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_tok",
                "expires_at": future_expires_at()
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/djinnos/server/pulls/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 42,
                "title": "feat: add feature",
                "state": "open",
                "merged": false,
                "html_url": "https://github.com/djinnos/server/pull/42",
                "head": { "ref": "feature-branch", "sha": "abc123" },
                "base": { "ref": "main", "sha": "def456" },
                "auto_merge": null,
                "node_id": "PR_abc123"
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex(r"/repos/djinnos/server/commits/abc123/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 1,
                "check_runs": [{
                    "id": 1,
                    "name": "ci",
                    "status": "completed",
                    "conclusion": "success",
                    "html_url": "https://github.com/checks/1"
                }]
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let (pr, checks) = client.get_pull_request("djinnos", "server", 42).await.unwrap();

        assert_eq!(pr.number, 42);
        assert_eq!(checks.total_count, 1);
        assert_eq!(checks.check_runs[0].conclusion.as_deref(), Some("success"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_pull_request_reviews_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_tok",
                "expires_at": future_expires_at()
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/djinnos/server/pulls/42/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "id": 1,
                    "user": { "login": "reviewer", "id": 999 },
                    "body": "LGTM!",
                    "created_at": "2024-01-01T00:00:00Z",
                    "updated_at": "2024-01-01T00:00:00Z",
                    "html_url": "https://github.com/djinnos/server/pull/42#comment-1",
                    "pull_request_review_id": 100,
                    "path": "src/lib.rs",
                    "line": 42
                }
            ])))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let comments = client
            .list_pull_request_reviews("djinnos", "server", 42)
            .await
            .unwrap();

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "LGTM!");
        assert_eq!(comments[0].user.as_ref().unwrap().login, "reviewer");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_pr_returns_error_on_422() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_tok",
                "expires_at": future_expires_at()
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/repos/djinnos/server/pulls"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "message": "Validation Failed",
                "errors": [{ "message": "A pull request already exists" }]
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let result = client
            .create_pull_request(
                "djinnos",
                "server",
                CreatePrParams {
                    title: "feat: dupe".into(),
                    body: "".into(),
                    head: "feature".into(),
                    base: "main".into(),
                    maintainer_can_modify: None,
                    draft: None,
                },
            )
            .await;

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("422"), "expected 422 in error: {}", msg);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn derive_installation_token_missing_creds_returns_error() {
        let repo = make_repo();
        // No tokens seeded.
        let client = GitHubApiClient::with_base_url(repo, "http://localhost:9999".into());
        let result = client.derive_installation_token().await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("No GitHub App tokens"), "unexpected error: {}", msg);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rate_limit_remaining_zero_returns_error() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        // Installation token is fetched first; give it a near-future reset.
        let reset_epoch = now_secs() + 1; // 1 second from now

        Mock::given(method("POST"))
            .and(path("/app/installations/12345678/access_tokens"))
            .respond_with(
                ResponseTemplate::new(201)
                    .append_header("X-RateLimit-Remaining", "0")
                    .append_header("X-RateLimit-Reset", &reset_epoch.to_string())
                    .set_body_json(serde_json::json!({
                        "token": "ghs_tok",
                        "expires_at": future_expires_at()
                    })),
            )
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let result = client.derive_installation_token().await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("rate limit"), "expected rate limit error: {}", msg);
    }
}
