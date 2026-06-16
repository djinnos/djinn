use djinn_core::models::{Task, TransitionAction};
use djinn_db::{ActivityQuery, SessionAuthRepository, TaskRepository, UserSettingsRepository};
use djinn_provider::github_api::{
    ActionsJob, CheckRun, DbBackedRefresher, GitHubApiClient, MergeMethod, PrReview,
    PrReviewFeedback, PrState, PullRequest, UserTokenExpired,
};
use djinn_provider::oauth::github_app_user;

use super::*;

/// Maximum number of review-fix rounds before escalating to Lead/Architect.
///
/// After this many PR review cycles without approval, the task is escalated
/// rather than re-dispatching a worker.
const PR_REVIEW_ROUND_THRESHOLD: u32 = 3;

/// Minimum seconds a task must have been in `pr_draft` before the poller will
/// check CI and potentially undraft it.  This prevents a race where the poller
/// runs before GitHub has registered the required check-runs for a newly-pushed
/// commit, sees an empty/stale check-run list, and incorrectly concludes CI
/// has passed.
const PR_DRAFT_MIN_AGE_SECS: i64 = 10;

/// Maximum consecutive merge failures before the poller invalidates its CI
/// cache and forces a full re-check.  This catches cases where CI failed
/// after we cached a "green" SHA, or where branch-protection rules block
/// the merge for reasons we didn't anticipate.
const MERGE_RETRY_RECHECK_THRESHOLD: u32 = 3;

/// Maximum number of distinct failing workflow runs whose jobs/steps are
/// aggregated into a single CI-failure rework comment. Failures frequently
/// span multiple workflow runs (e.g. separate `CI` and `Release` workflows on
/// the same SHA); we union all of them so the worker fixes every failure in one
/// pass instead of whack-a-mole. Bounded to keep the Actions API fan-out and
/// the comment size sane; if more runs failed we cap here and signal it.
const MAX_AGGREGATED_CI_RUNS: usize = 5;

/// Activity log event type for stored PR review feedback payloads.
///
/// Re-exported so the worker lifecycle layer can query for PR review feedback
/// without a module dependency on the coordinator's internal pr_poller.
pub const PR_REVIEW_FEEDBACK_EVENT: &str = "pr_review_feedback";

/// Activity log event type for per-cycle markers (used to count rounds).
const PR_REVIEW_CYCLE_EVENT: &str = "pr_review_cycle";

/// Activity log event type for per-cycle markers on the CI-failure rework path
/// (the analogue of `PR_REVIEW_CYCLE_EVENT` for the CI loop). Used to count how
/// many times a task has been kicked back for CI failures so the loop can
/// escalate instead of redispatching forever.
const PR_CI_CYCLE_EVENT: &str = "pr_ci_cycle";

/// Maximum CI-failure rework cycles before escalating to the Planner and
/// force-closing the task. Beyond this the worker is demonstrably unable to
/// turn the required checks green (commonly because the *real* failures are
/// non-required preview/deploy infra the diff can't touch), so looping is
/// pointless.
const PR_CI_FAILURE_THRESHOLD: u32 = 3;
mod ci_helpers;
mod conversation_resolution;
mod installation;
mod pr_commands;
mod pr_review_handlers;
mod pr_review_watcher;
mod pr_watcher;
mod state;

#[cfg(test)]
mod tests;

use crate::github_error_render::render_github_write_error;
use ci_helpers::{
    advisory_checks_section, blocking_failed_checks, build_ci_failure_sections, is_already_queued,
    is_merge_queue_405, parse_actions_run_id,
};
use conversation_resolution::{
    is_conversation_resolution_block, should_auto_resolve_conversations,
};
use installation::resolve_installation_client;
use pr_commands::enable_auto_merge_best_effort;
#[allow(unused_imports)]
pub(crate) use pr_commands::{
    AutoMergeTickDecision, decide_auto_merge_tick, record_auto_merge_decision_metrics,
};
use pr_review_handlers::effective_review_decision;
pub(crate) use pr_review_handlers::parse_pr_url;

#[cfg(test)]
use ci_helpers::is_advisory_check_name;
#[cfg(test)]
use pr_commands::{dequeue_reason_is_failure, dequeue_requires_rework};
#[cfg(test)]
use pr_review_handlers::{is_racing_unmerged_status, pick_conflict_blocker_sibling};
