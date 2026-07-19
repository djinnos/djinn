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

/// The repository's allowed merge methods, as reported by `GET /repos`.
///
/// GitHub exposes three independent per-repository toggles governing which
/// merge strategies the PUT `/merge` endpoint (and GitHub-managed auto-merge)
/// will accept. Djinn hardcoded squash historically, which wedges forever on
/// repos that disable squash. Each field defaults to `true` so a missing field
/// (older API surface / partial payload) degrades to the permissive legacy
/// assumption rather than falsely reporting a method as disallowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMergeConfig {
    #[serde(default = "default_true")]
    pub allow_squash_merge: bool,
    #[serde(default = "default_true")]
    pub allow_merge_commit: bool,
    #[serde(default = "default_true")]
    pub allow_rebase_merge: bool,
}

impl Default for RepoMergeConfig {
    fn default() -> Self {
        Self {
            allow_squash_merge: true,
            allow_merge_commit: true,
            allow_rebase_merge: true,
        }
    }
}

fn default_true() -> bool {
    true
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
    #[serde(default)]
    pub run_id: Option<u64>,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: String,
}

/// A GitHub Actions job within a workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionsJob {
    pub id: u64,
    #[serde(default)]
    pub run_id: Option<u64>,
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

/// A GitHub Actions workflow artifact from one bounded run-artifact page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionsArtifact {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub size_in_bytes: u64,
    #[serde(default)]
    pub expired: bool,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Exactly one repository-scoped artifact page; the provider never paginates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunArtifactsPage {
    pub total_count: u64,
    pub artifacts: Vec<ActionsArtifact>,
    pub truncated: bool,
}

/// Bounded compressed archive bytes and response metadata. ZIP extraction is
/// intentionally owned by the agent rather than the provider transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedArtifact {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
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

/// Provider-side result for required-check reproduction introspection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RequiredCheckReproduction {
    Reproducible(RequiredCheckReproductionContext),
    Unreproducible(RequiredCheckUnreproducible),
}

/// Structured context needed to reproduce a failing GitHub Actions check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequiredCheckReproductionContext {
    pub required_check_name: String,
    pub observed_head_sha: String,
    pub check_run_id: u64,
    pub workflow_run_id: u64,
    #[serde(default)]
    pub workflow_name: Option<String>,
    pub job: ReproductionJob,
    pub failing_step: ReproductionStep,
    pub command: String,
    pub setup_steps: Vec<ReproductionSetupStep>,
    pub log_tail: String,
}

/// The failing Actions job selected for reproduction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReproductionJob {
    pub id: u64,
    pub name: String,
    pub html_url: String,
}

/// The failing Actions step selected for reproduction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReproductionStep {
    pub number: u64,
    pub name: String,
}

/// A preceding shell command that the same workflow executed before the failing command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReproductionSetupStep {
    pub number: u64,
    pub name: String,
    pub command: String,
}

/// Typed reason why a required check could not be mapped to a local command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequiredCheckUnreproducible {
    pub required_check_name: String,
    pub observed_head_sha: String,
    pub reason: RequiredCheckUnreproducibleReason,
    #[serde(default)]
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequiredCheckUnreproducibleReason {
    CheckRunNotFound,
    CheckRunNotFailed,
    WorkflowRunNotFound,
    JobNotFound,
    FailingStepNotFound,
    CommandNotFound,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ActionsJobsResponse {
    pub(super) jobs: Vec<ActionsJob>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ActionsArtifactsResponse {
    pub(super) total_count: u64,
    pub(super) artifacts: Vec<ActionsArtifact>,
}

/// A GitHub Actions workflow run (subset). Used to locate the `merge_group`
/// run that rejected a PR — the merge-queue branch is ephemeral and the
/// dequeue event carries no ref, but the run persists with
/// `head_branch = gh-readonly-queue/.../pr-<number>-<sha>` and a `head_sha`
/// whose check runs also persist.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    #[serde(default)]
    pub workflow_id: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
    /// Path to the workflow definition in the repository, e.g.
    /// `.github/workflows/ci.yml`.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    pub head_sha: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct WorkflowRunsResponse {
    pub(super) workflow_runs: Vec<WorkflowRun>,
}

/// Request used to derive a repo-agnostic failure reproduction context from a
/// GitHub Actions run/check failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiFailureContextRequest {
    pub owner: String,
    pub repo: String,
    /// PR head SHA or merge-queue/run head SHA expected by the caller. Used to
    /// locate a run when `run_id` is not already known and returned as part of
    /// the bundle for stale-head checks.
    pub head_sha: String,
    /// Required check or job name that is currently failing.
    pub required_check_name: String,
    #[serde(default)]
    pub workflow_run_id: Option<u64>,
    #[serde(default)]
    pub workflow_id: Option<u64>,
    #[serde(default)]
    pub job_id: Option<u64>,
    /// Optional workflow file path when the caller already knows it.
    #[serde(default)]
    pub workflow_path: Option<String>,
}

/// A setup step preceding the failed step in the same Actions job. `run:`
/// scripts are directly locally reproducible by later coordinator logic; `uses:`
/// actions are retained as generic setup context without inventing a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiSetupStep {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses: Option<String>,
}

/// Provider-facing context bundle for a failing required GitHub Actions check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiFailureContextBundle {
    pub owner: String,
    pub repo: String,
    pub required_check_name: String,
    pub workflow_run_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_path: Option<String>,
    pub job_id: u64,
    pub job_name: String,
    pub failing_step_name: String,
    pub failing_step_number: u64,
    pub step_script: String,
    pub setup_steps: Vec<CiSetupStep>,
    pub log_tail: String,
    pub observed_head_sha: String,
}

/// A file changed in a PR (subset of GitHub's PR file response).
#[derive(Debug, Clone, Deserialize)]
pub struct PrFile {
    pub sha: String,
    pub filename: String,
    pub status: String, // "added", "removed", "modified", "renamed"
    pub additions: u32,
    pub deletions: u32,
    pub changes: u32,
    /// The unified-diff patch for this file, as returned by GitHub's
    /// `GET /pulls/{n}/files` endpoint. Present when the file is a text
    /// file and the diff is under 300 files / per-file byte limits.
    #[serde(default)]
    pub patch: Option<String>,
}
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
    #[serde(default)]
    pub user: Option<GitHubUser>,
    pub merged: Option<bool>,
    /// The SHA of the merge commit on the base branch once the PR is merged.
    /// GitHub populates this with the real landed commit when `merged == true`
    /// (and with a speculative test-merge SHA while the PR is still open, which
    /// we only ever read on the `merged` path). `None` when GitHub omits it.
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
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

/// Response from `GET /branches/{branch}/protection/required_status_checks`.
///
/// `contexts` is the list of required status-check context names (the
/// merge-gating checks). Advisory checks are not present here.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct RequiredStatusChecksResponse {
    #[serde(default)]
    pub(super) contexts: Vec<String>,
}

/// Subset of the `GET /compare/{base}...{head}` response we care about:
/// how many commits the head is ahead of the base.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct CompareResponse {
    pub(super) ahead_by: u64,
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

/// State of a pull request's entry in the repository merge queue.
///
/// Maps to GitHub GraphQL `MergeQueueEntryState`. The PR transitions through
/// these as the queue processes it; observing `Unmergeable` is the only state
/// that signals failure — the rest are normal progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeQueueEntryState {
    Queued,
    AwaitingChecks,
    Locked,
    Mergeable,
    Unmergeable,
}

/// A pull request's current entry in the merge queue.
///
/// Returned by `get_pr_merge_queue_state` when the PR has been accepted into
/// a queue. Absence (the wrapping `Option` being `None`) means the PR is not
/// currently queued — either it hasn't been enqueued, it merged, or it was
/// kicked out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeQueueEntry {
    /// GraphQL node ID for this entry. Used for `dequeue_pull_request`.
    pub id: String,
    pub state: MergeQueueEntryState,
    /// 1-based position in the queue.
    #[serde(default)]
    pub position: Option<u32>,
    /// Estimated seconds until the entry reaches the front of the queue.
    #[serde(default)]
    pub estimated_time_to_merge: Option<u32>,
    /// True when the queue had to bisect down to running this PR alone
    /// (other PRs in the original group were innocent).
    #[serde(default)]
    pub solo: Option<bool>,
}

/// Status of an active auto-merge request on a PR.
///
/// Set by `enable_auto_merge`. When this is `Some`, GitHub is watching for
/// the PR to become mergeable and will enqueue/merge automatically — djinn
/// just polls for completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoMergeRequest {
    /// ISO-8601 timestamp when auto-merge was enabled.
    pub enabled_at: Option<String>,
    /// `SQUASH`, `MERGE`, or `REBASE` — uppercase, as returned by GraphQL.
    pub merge_method: Option<String>,
}

/// Combined merge-queue / auto-merge state for a PR.
///
/// Returned by `get_pr_merge_queue_state`. Use this in the poller to decide
/// whether to wait, treat as failure, or fall back to manual merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrMergeQueueState {
    /// `CLEAN`, `BLOCKED`, `BEHIND`, `UNSTABLE`, `HAS_HOOKS`, `DIRTY`,
    /// `UNKNOWN` — uppercase as returned by GraphQL. `BLOCKED` typically
    /// means branch protection requires something we haven't satisfied yet.
    pub merge_state_status: Option<String>,
    /// `Some` once a queue has accepted the PR.
    pub merge_queue_entry: Option<MergeQueueEntry>,
    /// `Some` once `enable_auto_merge` has been called.
    pub auto_merge_request: Option<AutoMergeRequest>,
    /// Most recent dequeue event from the PR timeline, if any. Populated
    /// when the queue evicted the PR — carries the reason and links to the
    /// failing check runs on the merge-group ref.
    pub last_dequeue: Option<DequeueEvent>,
    /// Timestamp of the PR's current head commit (`pushedDate` when GitHub
    /// provides it, else `committedDate`). Comparing this against
    /// `last_dequeue.created_at` tells whether rework landed after the queue
    /// rejected the PR — both are RFC3339 UTC strings, so a lexicographic
    /// compare is a chronological compare.
    pub head_committed_at: Option<String>,
}

/// A `DequeuedEvent` from the PR timeline — emitted when GitHub removes a PR
/// from the merge queue (kicked out, manually dequeued, or branch updated).
///
/// `reason` is a free-form string from GitHub. Known values include
/// `"BRANCH_INVALIDATED"`, `"CHECKS_FAILED"`, `"DEQUEUED"`, `"MERGE_CONFLICT"`,
/// `"NO_RESPONSE"`, `"NOT_QUEUEABLE"`, `"QUEUE_CLEARED"`,
/// `"ROLL_BACK"`, `"UNKNOWN_REMOVAL_REASON"` (matched verbatim from GraphQL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DequeueEvent {
    pub reason: Option<String>,
    /// Merge-group ref the queue was running against when the dequeue
    /// happened (e.g. `refs/heads/gh-readonly-queue/main/pr-524-abc…`).
    /// Used to look up the failing check runs.
    pub merge_group_ref: Option<String>,
    pub created_at: Option<String>,
    /// `beforeCommit.oid` from the removal event. NOTE: empirically this is
    /// the head of the *merge group* the queue was running (a synthetic
    /// `gh-readonly-queue/...` commit), NOT the PR's head SHA — do not
    /// compare it against the PR head to detect rework; use
    /// `PrMergeQueueState::head_committed_at` vs `created_at` instead.
    pub before_commit_sha: Option<String>,
}
