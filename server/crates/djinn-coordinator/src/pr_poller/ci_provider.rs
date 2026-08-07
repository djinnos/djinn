//! The substitutable provider boundary the CI-route lane executors call
//! through (proposal `nafu`, wave 3b).
//!
//! # Why a trait and not a `#[cfg(test)]` mirror
//!
//! `PrDraftCiAction` next door is a `#[cfg(test)]` enum mirroring an inlined
//! production `match`, and that pattern is cheap and legitimate — for a *pure
//! decision*. Mirroring it here would be a category error. The thing the spec's
//! fixtures have to count is "did the executor mutate provider state", and a
//! test-only mirror of the call site proves a property of the mirror, not of
//! the executor: the production path would still reach the concrete
//! `GitHubApiClient` and there would be nothing for a fixture to count.
//!
//! So the seam has to be one the production executor genuinely goes through.
//! It is a trait, deliberately the narrowest one that makes the fixtures
//! writable: the calls the two lane executors make and nothing else. There is
//! already precedent for exactly this shape one file over —
//! [`super::pr_cleanup::PrCleanupGitHub`] is a three-method `#[async_trait]`
//! over the same concrete client — so this introduces no new idiom.
//!
//! # What it is not
//!
//! It is not an abstraction over `GitHubApiClient`. Every method here forwards
//! to the inherent method of the same name with the same arguments and the same
//! error type; the impl block below is the whole implementation. No call site
//! outside the lane executors is rewritten to use it, no behaviour changes, and
//! the concrete client remains the type every other poller path holds.
//!
//! # The fourth method
//!
//! The task named three calls. The merge-group executor makes a fourth —
//! `get_check_run_annotations` — because it is the *producer* of the routing
//! contract's `LogApiError` input, which has no producer anywhere else in the
//! tree. Leaving it off the seam would leave that input unreachable from a
//! fixture, which is the blocker this module exists to remove.

use async_trait::async_trait;

use djinn_provider::github_api::{
    CheckAnnotation, CheckRunsResponse, GitHubApiClient, GitHubApiError, MergeMethod,
};

/// The exact provider surface the CI-route lane executors touch.
///
/// `Send + Sync` because a lane executor runs inside the coordinator's tick and
/// the client is shared across the poll.
#[async_trait]
pub(crate) trait CiRouteProvider: Send + Sync {
    /// Tier 1, PR-head lane. `POST /repos/{owner}/{repo}/actions/runs/{run_id}/rerun-failed-jobs`.
    async fn rerun_failed_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<(), GitHubApiError>;

    /// Tier 1, merge-group lane. GraphQL `enablePullRequestAutoMerge`.
    #[allow(clippy::too_many_arguments)]
    async fn enable_auto_merge(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        method: MergeMethod,
        node_id: &str,
        commit_headline: &str,
    ) -> Result<serde_json::Value, GitHubApiError>;

    /// Evidence capture for a ref whose immutable run identity is already
    /// known. A failure here is the routing contract's `CheckApiError`.
    async fn list_check_runs_for_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CheckRunsResponse, GitHubApiError>;

    /// Annotation capture for the primary blocking check. A failure here is the
    /// routing contract's `LogApiError`.
    async fn get_check_run_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
    ) -> Result<Vec<CheckAnnotation>, GitHubApiError>;
}

#[async_trait]
impl CiRouteProvider for GitHubApiClient {
    async fn rerun_failed_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<(), GitHubApiError> {
        GitHubApiClient::rerun_failed_jobs(self, owner, repo, run_id).await
    }

    async fn enable_auto_merge(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
        method: MergeMethod,
        node_id: &str,
        commit_headline: &str,
    ) -> Result<serde_json::Value, GitHubApiError> {
        GitHubApiClient::enable_auto_merge(
            self,
            owner,
            repo,
            pull_number,
            method,
            node_id,
            commit_headline,
        )
        .await
    }

    async fn list_check_runs_for_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<CheckRunsResponse, GitHubApiError> {
        GitHubApiClient::list_check_runs_for_ref(self, owner, repo, git_ref).await
    }

    async fn get_check_run_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: u64,
    ) -> Result<Vec<CheckAnnotation>, GitHubApiError> {
        GitHubApiClient::get_check_run_annotations(self, owner, repo, check_run_id).await
    }
}
