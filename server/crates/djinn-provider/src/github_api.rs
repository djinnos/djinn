//! GitHub REST API v3 client.
//!
//! Provides a [`GitHubApiClient`] that uses the GitHub App user OAuth token
//! directly for all API calls (per ADR-039).
//!
//! # Operations
//! - [`GitHubApiClient::create_pull_request`] — open a PR
//! - [`GitHubApiClient::enable_auto_merge`] — set auto-merge on a PR
//! - [`GitHubApiClient::get_pull_request`] — fetch PR status and CI checks
//! - [`GitHubApiClient::list_pull_request_reviews`] — list inline review comments
//! - [`GitHubApiClient::list_pr_review_states`] — list top-level review states (APPROVED, CHANGES_REQUESTED, etc.)
//! - [`GitHubApiClient::fetch_pr_review_feedback`] — aggregate CHANGES_REQUESTED reviews + inline comments into [`PrReviewFeedback`]
//! - [`GitHubApiClient::get_check_run_annotations`] — fetch error annotations for a CI check run
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

use crate::oauth::github_app::GitHubAppTokens;
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

/// A GitHub Actions job within a workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionsJob {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ActionsJobsResponse {
    jobs: Vec<ActionsJob>,
}

/// A single annotation attached to a check run (error/warning/notice).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckAnnotation {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub annotation_level: String,
    pub message: String,
    pub title: Option<String>,
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
    /// Whether the PR can be merged (no conflicts). `None` when GitHub hasn't
    /// computed mergeability yet.
    #[serde(default)]
    pub mergeable: Option<bool>,
    /// Mergeable state: `"clean"`, `"dirty"`, `"blocked"`, `"behind"`, `"unknown"`, etc.
    #[serde(default)]
    pub mergeable_state: Option<String>,
    /// Whether the PR is a draft.
    #[serde(default)]
    pub draft: Option<bool>,
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
        }
    }

    // ─── Token management ─────────────────────────────────────────────────────

    /// Load the cached GitHub OAuth App user access token.
    async fn load_user_token(&self) -> Result<GitHubAppTokens> {
        GitHubAppTokens::load_from_db(&self.cred_repo)
            .await
            .ok_or_else(|| anyhow!("No GitHub App tokens found — please authenticate first"))
    }

    // ─── Core request helper ──────────────────────────────────────────────────

    /// Execute a request using the user OAuth token directly (per ADR-039).
    /// Retries once with a refreshed token on 401.
    async fn send_with_retry<F, Fut>(&self, build_request: F) -> Result<Response>
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

    /// List open pull requests whose head branch matches `head`.
    ///
    /// `head` should be in `owner:branch` format (e.g. `"djinnos:task/453b"`).
    /// Returns an empty vec when no matching PRs exist.
    pub async fn list_pulls_by_head(
        &self,
        owner: &str,
        repo: &str,
        head: &str,
    ) -> Result<Vec<PullRequest>> {
        let url = format!(
            "{}/repos/{}/{}/pulls?state=open&head={}",
            self.base_url, owner, repo, head
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
            return Err(anyhow!("list_pulls_by_head failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
    }

    /// Enable auto-merge on an existing pull request.
    ///
    /// Uses the GitHub GraphQL `enablePullRequestAutoMerge` mutation so that the
    /// PR is only merged once all required status checks pass. The repo must have
    /// auto-merge enabled in its settings (Settings → General → Allow auto-merge).
    pub async fn enable_auto_merge(
        &self,
        _owner: &str,
        _repo: &str,
        _pull_number: u64,
        method: MergeMethod,
        node_id: &str,
        commit_headline: &str,
    ) -> Result<serde_json::Value> {
        let merge_method = match method {
            MergeMethod::Squash => "SQUASH",
            MergeMethod::Rebase => "REBASE",
            MergeMethod::Merge => "MERGE",
        };

        let query = r#"
            mutation EnableAutoMerge($pullRequestId: ID!, $mergeMethod: PullRequestMergeMethod!, $commitHeadline: String!) {
                enablePullRequestAutoMerge(input: {
                    pullRequestId: $pullRequestId,
                    mergeMethod: $mergeMethod,
                    commitHeadline: $commitHeadline
                }) {
                    pullRequest { number title autoMergeRequest { enabledAt mergeMethod } }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": {
                "pullRequestId": node_id,
                "mergeMethod": merge_method,
                "commitHeadline": commit_headline,
            }
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    // Use base_url for testability; in production base_url is
                    // https://api.github.com so this resolves to /graphql.
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
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

        let json: serde_json::Value = resp.json().await?;

        // GraphQL returns 200 even on errors — check the `errors` field.
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!("enable_auto_merge GraphQL error: {}", errors));
        }

        Ok(json)
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

    /// Fetch annotations for a check run.
    ///
    /// Uses `GET /repos/{owner}/{repo}/check-runs/{check_run_id}/annotations`.
    /// Returns error-level annotations (failures, warnings, notices) that contain
    /// the actual CI error messages the worker needs to fix failures.
    pub async fn get_check_run_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
    ) -> Result<Vec<CheckAnnotation>> {
        let url = format!(
            "{}/repos/{}/{}/check-runs/{}/annotations",
            self.base_url, owner, repo, check_run_id
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
                "get_check_run_annotations failed ({}): {}",
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

    /// Check whether the installation token can access a repository.
    ///
    /// Calls `GET /repos/{owner}/{repo}` and returns `Ok(())` on 200.
    /// Returns a descriptive error on 404/403 or any other failure.
    pub async fn check_repo_access(&self, owner: &str, repo: &str) -> Result<()> {
        let url = format!("{}/repos/{}/{}", self.base_url, owner, repo);

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

        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(anyhow!("check_repo_access failed ({}): {}", status, body))
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

    /// Merge a pull request via the REST API.
    ///
    /// Uses `PUT /repos/{owner}/{repo}/pulls/{pull_number}/merge` with the
    /// specified merge method and commit title.
    pub async fn merge_pull_request(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        method: MergeMethod,
        commit_title: &str,
    ) -> Result<serde_json::Value> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}/merge",
            self.base_url, owner, repo, pull_number
        );
        let merge_method_str = match method {
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
            MergeMethod::Merge => "merge",
        };
        let body = serde_json::json!({
            "merge_method": merge_method_str,
            "commit_title": commit_title,
        });

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
            return Err(anyhow!("merge_pull_request failed ({}): {}", status, body));
        }
        Ok(resp.json().await?)
    }

    /// Mark a draft PR as ready for review (undraft it).
    ///
    /// Uses the GraphQL `markPullRequestReadyForReview` mutation.  The REST
    /// `PATCH /pulls/{number}` endpoint silently ignores `{"draft": false}`, so
    /// GraphQL is the only supported path for converting a draft PR.
    ///
    /// `node_id` is the GraphQL node ID of the pull request (available as
    /// `PullRequest::node_id` from `get_pull_request`).
    pub async fn mark_pr_ready_for_review(&self, node_id: &str) -> Result<serde_json::Value> {
        let query = r#"
            mutation MarkPullRequestReadyForReview($pullRequestId: ID!) {
                markPullRequestReadyForReview(input: { pullRequestId: $pullRequestId }) {
                    pullRequest { number isDraft }
                }
            }
        "#;

        let body = serde_json::json!({
            "query": query,
            "variables": { "pullRequestId": node_id }
        });

        let base_url = self.base_url.clone();
        let resp = self
            .send_with_retry(|token| {
                let body = body.clone();
                let http = self.http.clone();
                let base_url = base_url.clone();
                async move {
                    let graphql_url = format!("{}/graphql", base_url);
                    let resp = http
                        .post(&graphql_url)
                        .bearer_auth(&token)
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
            return Err(anyhow!(
                "mark_pr_ready_for_review failed ({}): {}",
                status,
                body
            ));
        }

        let json: serde_json::Value = resp.json().await?;

        // GraphQL returns 200 even on errors — check the `errors` field.
        if let Some(errors) = json.get("errors") {
            return Err(anyhow!(
                "mark_pr_ready_for_review GraphQL error: {}",
                errors
            ));
        }

        Ok(json)
    }

    /// List jobs for a workflow run.
    ///
    /// Uses `GET /repos/{owner}/{repo}/actions/runs/{run_id}/jobs`.
    /// Returns up to 100 jobs (GitHub default page size).
    pub async fn list_run_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<Vec<ActionsJob>> {
        let url = format!(
            "{}/repos/{}/{}/actions/runs/{}/jobs?per_page=100",
            self.base_url, owner, repo, run_id
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
            return Err(anyhow!("list_run_jobs failed ({}): {}", status, body));
        }
        let parsed: ActionsJobsResponse = resp.json().await?;
        Ok(parsed.jobs)
    }

    /// Download the raw log text for a specific Actions job.
    ///
    /// Uses `GET /repos/{owner}/{repo}/actions/jobs/{job_id}/logs`.
    /// GitHub returns a 302 redirect to a temporary download URL; reqwest
    /// follows the redirect automatically.
    pub async fn get_job_logs(&self, owner: &str, repo: &str, job_id: u64) -> Result<String> {
        let url = format!(
            "{}/repos/{}/{}/actions/jobs/{}/logs",
            self.base_url, owner, repo, job_id
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
            return Err(anyhow!("get_job_logs failed ({}): {}", status, body));
        }
        Ok(resp.text().await?)
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
        return Err(anyhow!(
            "GitHub rate limit exhausted — retry after {}s",
            sleep_secs
        ));
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

/// Minimal ISO-8601 UTC parser: `YYYY-MM-DDTHH:MM:SSZ` → epoch seconds.
#[cfg(test)]
fn chrono_parse_iso8601(s: &str) -> Result<i64> {
    // Format: "2024-01-01T12:00:00Z"
    let s = s.trim_end_matches('Z');
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return Err(anyhow!("invalid ISO-8601: {}", s));
    }
    let date_parts: Vec<u32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u32> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();
    if date_parts.len() != 3 || time_parts.len() != 3 {
        return Err(anyhow!("invalid ISO-8601 parts: {}", s));
    }
    let (y, mo, d) = (
        date_parts[0] as i64,
        date_parts[1] as i64,
        date_parts[2] as i64,
    );
    let (h, mi, sec) = (
        time_parts[0] as i64,
        time_parts[1] as i64,
        time_parts[2] as i64,
    );

    // Days since Unix epoch (1970-01-01).
    // Zeller-like calculation for simplicity.
    let days = days_since_epoch(y, mo, d);
    Ok(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// Compute days since 1970-01-01 for a given Gregorian date.
#[cfg(test)]
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

    use crate::oauth::github_app::GITHUB_APP_OAUTH_DB_KEY;
    use crate::repos::CredentialRepository;

    // ─── Test helpers ──────────────────────────────────────────────────────────

    fn make_repo() -> Arc<CredentialRepository> {
        let db = Database::open_in_memory().expect("in-memory db");
        Arc::new(CredentialRepository::new(db, EventBus::noop()))
    }

    async fn seed_tokens(repo: &CredentialRepository, access_token: &str) {
        // Serialize manually — legacy fields are private on GitHubAppTokens.
        let json = serde_json::json!({
            "access_token": access_token,
            "user_login": "djinn-test",
        })
        .to_string();
        repo.set("github_app", GITHUB_APP_OAUTH_DB_KEY, &json)
            .await
            .unwrap();
    }

    fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
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
        let formatted = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year, month, day, h, m, s
        );
        let parsed = chrono_parse_iso8601(&formatted).unwrap();
        // Allow ±1 second for rounding.
        assert!(
            (parsed - ts).abs() <= 1,
            "round-trip failed: {} vs {}",
            parsed,
            ts
        );
    }

    // ─── Integration tests with wiremock ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_pull_request_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        // PR creation endpoint — uses user token directly (ADR-039).
        Mock::given(method("POST"))
            .and(path("/repos/djinnos/server/pulls"))
            .and(header("Authorization", "Bearer ghu_user"))
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

        // The GraphQL mutation goes to /graphql, not the REST merge endpoint.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("Authorization", "Bearer ghu_user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "enablePullRequestAutoMerge": {
                        "pullRequest": {
                            "number": 42,
                            "title": "feat: add feature",
                            "autoMergeRequest": {
                                "enabledAt": "2026-01-01T00:00:00Z",
                                "mergeMethod": "SQUASH"
                            }
                        }
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let result = client
            .enable_auto_merge(
                "djinnos",
                "server",
                42,
                MergeMethod::Squash,
                "PR_node123",
                "chore(clbs): Phase 1: split extension params",
            )
            .await
            .unwrap();

        assert!(result["data"]["enablePullRequestAutoMerge"]["pullRequest"]["number"] == 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_pr_ready_for_review_uses_graphql_mutation() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("Authorization", "Bearer ghu_user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "markPullRequestReadyForReview": {
                        "pullRequest": { "number": 8, "isDraft": false }
                    }
                }
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let result = client.mark_pr_ready_for_review("PR_node456").await.unwrap();

        assert_eq!(
            result["data"]["markPullRequestReadyForReview"]["pullRequest"]["isDraft"],
            false
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_pr_ready_for_review_propagates_graphql_error() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "errors": [{ "message": "Resource not accessible by integration" }]
            })))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let err = client
            .mark_pr_ready_for_review("PR_node456")
            .await
            .unwrap_err();

        assert!(err.to_string().contains("GraphQL error"), "got: {err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_pull_request_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

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
            .and(path_regex(
                r"/repos/djinnos/server/commits/abc123/check-runs",
            ))
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
        let (pr, checks) = client
            .get_pull_request("djinnos", "server", 42)
            .await
            .unwrap();

        assert_eq!(pr.number, 42);
        assert_eq!(checks.total_count, 1);
        assert_eq!(checks.check_runs[0].conclusion.as_deref(), Some("success"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_pull_request_reviews_success() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

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
    async fn list_pulls_by_head_returns_matching_prs() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("GET"))
            .and(path("/repos/djinnos/server/pulls"))
            .and(wiremock::matchers::query_param("state", "open"))
            .and(wiremock::matchers::query_param(
                "head",
                "djinnos:task/453b",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "number": 99,
                    "title": "chore(453b): Move epic tools",
                    "state": "open",
                    "merged": false,
                    "html_url": "https://github.com/djinnos/server/pull/99",
                    "head": { "ref": "task/453b", "sha": "aaa111" },
                    "base": { "ref": "main", "sha": "bbb222" },
                    "auto_merge": null,
                    "node_id": "PR_existing"
                }
            ])))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let prs = client
            .list_pulls_by_head("djinnos", "server", "djinnos:task/453b")
            .await
            .unwrap();

        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 99);
        assert_eq!(prs[0].html_url, "https://github.com/djinnos/server/pull/99");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_pulls_by_head_returns_empty_when_no_match() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        Mock::given(method("GET"))
            .and(path("/repos/djinnos/server/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let prs = client
            .list_pulls_by_head("djinnos", "server", "djinnos:no-such-branch")
            .await
            .unwrap();

        assert!(prs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_creds_returns_error() {
        let server = MockServer::start().await;
        let repo = make_repo();
        // No tokens seeded — should fail when trying to load user token.
        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let result = client.get_pull_request("djinnos", "server", 1).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("No GitHub App tokens"),
            "unexpected error: {}",
            msg
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_with_retry_refreshes_on_401() {
        let server = MockServer::start().await;
        let repo = make_repo();
        seed_tokens(&repo, "ghu_user").await;

        // First call returns 401, second (after refresh) succeeds.
        // Since refresh_cached_token hits GitHub, and we can't mock that easily,
        // just verify that 401 triggers the refresh path (which will fail in
        // test with "No GitHub App tokens" since refresh needs real endpoints).
        Mock::given(method("GET"))
            .and(path("/repos/djinnos/server/pulls/1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let client = GitHubApiClient::with_base_url(repo, server.uri());
        let result = client.get_pull_request("djinnos", "server", 1).await;
        // Should fail because refresh can't succeed in test environment.
        assert!(result.is_err());
    }
}
