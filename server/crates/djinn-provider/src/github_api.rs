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
mod contents;
mod error;
mod pull_requests;
pub mod refresh;
mod repo_config;
mod reviews;
pub mod search;
#[cfg(test)]
mod tests;
mod transport;
mod types;
mod write_errors;

use std::sync::Arc;

use reqwest::Client;
use tokio::sync::RwLock;

pub use error::{GitHubApiError, GitHubErrorSource};
pub use refresh::{DbBackedRefresher, NoRefresh, UserTokenRefresh};
pub use transport::UserTokenExpired;
pub use types::{
    ActionsArtifact, ActionsJob, ActionsJobStep, AutoMergeRequest, CheckAnnotation, CheckRun,
    CheckRunsResponse, CiFailureContextBundle, CiFailureContextRequest, CiSetupStep,
    CreatePrParams, DequeueEvent, DownloadedArtifact, GitHubUser, MergeMethod, MergeQueueEntry,
    MergeQueueEntryState, PrFile, PrMergeQueueState, PrRef, PrReview, PrReviewFeedback, PrState,
    PullRequest, RepoMergeConfig, ReproductionJob, ReproductionSetupStep, ReproductionStep,
    RequiredCheckReproduction, RequiredCheckReproductionContext, RequiredCheckUnreproducible,
    RequiredCheckUnreproducibleReason, ReviewComment, RunArtifactsPage, WorkflowRun,
};
pub use write_errors::{
    GitHubWriteErrorEnvelope, GitHubWriteErrorInput, github_write_error_envelope,
};

/// GitHub REST API v3 base URL.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// GitHub REST API v3 client.
///
/// Holds a reference to the credential repository for loading cached OAuth
/// tokens, and an optional override for the API base URL (used in tests).
/// How the GitHub API client authenticates outbound requests.
///
/// The legacy user-token path has been removed — every MCP request now
/// threads the caller's GitHub access token through the
/// `djinn_core::auth_context::SESSION_USER_TOKEN` task-local, and
/// non-user server paths must go through the `Installation` variant (which
/// mints App installation tokens attributed to `djinn-bot[bot]`).
#[derive(Clone)]
pub(super) enum AuthMode {
    /// Read the authenticated user's GitHub access token from the
    /// per-request task-local. Used by MCP tools invoked via HTTP.
    SessionUser,
    /// Mint a GitHub App installation token for each call.
    Installation { installation_id: u64 },
    /// Use a specific user GitHub access token supplied at client
    /// construction. For background coordinator paths (pr_poller,
    /// schedulers) that act on behalf of a user but run outside any
    /// HTTP `SESSION_USER_TOKEN` scope.
    ///
    /// On 401 the transport consults `refresher`:
    /// * If it returns a new access token, the call retries once with
    ///   the rotated token and `current` is overwritten in place so
    ///   subsequent calls pick up the fresh credential without a DB
    ///   round-trip.
    /// * Otherwise the call surfaces [`UserTokenExpired`] and the
    ///   caller decides whether to skip or bounce the user to login.
    UserToken {
        /// Lock so a refresh on one in-flight request also benefits
        /// the next. Cheap: critical sections only clone the string.
        current: Arc<RwLock<String>>,
        refresher: Arc<dyn UserTokenRefresh>,
    },
}

#[derive(Clone)]
pub struct GitHubApiClient {
    pub(super) http: Client,
    pub(super) auth: AuthMode,
    /// Override for the GitHub API base URL (default: `GITHUB_API_BASE`).
    pub(super) base_url: String,
}

impl GitHubApiClient {
    /// Create a client that reads the caller's GitHub user access token from
    /// the per-request task-local (`SESSION_USER_TOKEN`). Requests made
    /// outside an active MCP scope will fail with a "sign in" error.
    pub fn for_session_user() -> Self {
        Self::build(AuthMode::SessionUser, GITHUB_API_BASE.to_string())
    }

    /// Create a client that authenticates as a GitHub App installation.
    ///
    /// Each call mints (or reuses) an installation access token via
    /// [`crate::github_app::get_installation_token`], so actions are
    /// attributed to the App's bot identity (`djinn-bot[bot]`) rather than
    /// to an authenticated user.
    pub fn for_installation(installation_id: u64) -> Self {
        Self::build(
            AuthMode::Installation { installation_id },
            GITHUB_API_BASE.to_string(),
        )
    }

    /// Installation-token constructor with a custom base URL (useful for
    /// tests and self-hosted GitHub Enterprise).
    pub fn for_installation_with_base_url(installation_id: u64, base_url: String) -> Self {
        Self::build(AuthMode::Installation { installation_id }, base_url)
    }

    /// Create a client that authenticates with a literal user GitHub access
    /// token. For background paths (pr_poller, scheduled jobs) that need to
    /// act as a specific user but cannot read `SESSION_USER_TOKEN` because
    /// they run outside an HTTP request.
    ///
    /// **No refresh on 401** — the token is treated as opaque/static.
    /// Production paths should prefer [`Self::for_user_session`], which
    /// rotates via the paired refresh token instead of bouncing the
    /// user back to `/auth/github/start` on every 8-hour cycle.
    pub fn for_user_token(token: String) -> Self {
        Self::build(
            AuthMode::UserToken {
                current: Arc::new(RwLock::new(token)),
                refresher: Arc::new(NoRefresh),
            },
            GITHUB_API_BASE.to_string(),
        )
    }

    /// User-token constructor with a custom base URL (useful for tests).
    pub fn for_user_token_with_base_url(token: String, base_url: String) -> Self {
        Self::build(
            AuthMode::UserToken {
                current: Arc::new(RwLock::new(token)),
                refresher: Arc::new(NoRefresh),
            },
            base_url,
        )
    }

    /// Create a client backed by a refreshable user session. On `401`
    /// the transport rotates the access/refresh pair via
    /// [`crate::oauth::github_app_user::refresh_user_token`] and retries
    /// once before giving up.
    pub fn for_user_session(token: String, refresher: Arc<dyn UserTokenRefresh>) -> Self {
        Self::build(
            AuthMode::UserToken {
                current: Arc::new(RwLock::new(token)),
                refresher,
            },
            GITHUB_API_BASE.to_string(),
        )
    }

    /// Refreshable-session constructor with a custom base URL (used in
    /// transport tests that mock the GitHub endpoint).
    pub fn for_user_session_with_base_url(
        token: String,
        refresher: Arc<dyn UserTokenRefresh>,
        base_url: String,
    ) -> Self {
        Self::build(
            AuthMode::UserToken {
                current: Arc::new(RwLock::new(token)),
                refresher,
            },
            base_url,
        )
    }

    fn build(auth: AuthMode, base_url: String) -> Self {
        let http = Client::builder()
            .user_agent("djinn-server/0.1 (+https://github.com/djinnos/server)")
            .build()
            .expect("failed to build reqwest client");

        Self {
            http,
            auth,
            base_url,
        }
    }
}
