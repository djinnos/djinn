use serde::{Deserialize, Serialize};

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
    /// The workflow file name (e.g. `quality-gate.yml`).
    #[serde(default)]
    pub workflow_name: Option<String>,
    /// Individual steps within the job.
    #[serde(default)]
    pub steps: Vec<ActionsJobStep>,
}

/// A single step within a GitHub Actions job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionsJobStep {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    /// 1-based step number within the job.
    pub number: u64,
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
