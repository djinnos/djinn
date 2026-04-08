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
//! - [`GitHubApiClient::list_run_jobs`] — list GitHub Actions jobs for a workflow run
//! - [`GitHubApiClient::get_job_logs`] — download raw GitHub Actions job logs
//!
//! # Token lifecycle
//! On every API call the client loads the cached user token before issuing the
//! request. On a `401 Unauthorized` response, the client surfaces a
//! re-authentication error.
//!
//! # Rate limiting
//! Responses that carry `X-RateLimit-Remaining: 0` cause the client to
//! sleep until `X-RateLimit-Reset` (epoch seconds) before returning an error.
//! If the header is absent the client falls back to exponential back-off on
//! `429 Too Many Requests`.

mod checks;
mod pull_requests;
mod reviews;
#[cfg(test)]
mod tests;
mod transport;
mod types;

use reqwest::Client;
use std::sync::Arc;

use crate::repos::CredentialRepository;

pub use types::{
    ActionsJob, ActionsJobStep, CheckAnnotation, CheckRun, CheckRunsResponse, CreatePrParams,
    GitHubUser, MergeMethod, PrRef, PrReview, PrReviewFeedback, PrState, PullRequest,
    ReviewComment,
};

/// GitHub REST API v3 base URL.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// GitHub REST API v3 client.
///
/// Holds a reference to the credential repository for loading cached OAuth
/// tokens, and an optional override for the API base URL (used in tests).
#[derive(Clone)]
pub struct GitHubApiClient {
    pub(super) http: Client,
    pub(super) cred_repo: Arc<CredentialRepository>,
    /// Override for the GitHub API base URL (default: `GITHUB_API_BASE`).
    pub(super) base_url: String,
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
}
