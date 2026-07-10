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
/// Re-exported from `djinn-orchestration-types` so the worker lifecycle layer
/// can query for PR review feedback without a module dependency on the
/// coordinator's internal pr_poller.
pub use djinn_orchestration_types::coordinator::PR_REVIEW_FEEDBACK_EVENT;

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

/// Maximum consecutive identical CI-failure fingerprints before escalating to
/// the Planner. Lower than PR_CI_FAILURE_THRESHOLD (3) so content-aware
/// escalation fires first, giving the Planner a chance to intervene before
/// the blind cycle-count force-close kicks in.
const SAME_CI_SIGNATURE_THRESHOLD: u32 = 2;

/// Activity log event type for per-fingerprint tracking markers.
const SAME_CI_SIGNATURE_EVENT: &str = "same_ci_signature";

/// Minimum minutes a task must have been in `needs_task_review` with a red PR
/// and an unchanged head SHA before the review-stuck trigger fires. Prevents
/// escalating a PR that is mid-CI or has a pending check-run.
const REVIEW_STUCK_WINDOW_MINUTES: i64 = 10;
mod ci_failure_analysis;
mod ci_helpers;
mod conversation_resolution;
mod installation;
mod merged_change_projection;
pub(crate) mod pr_cleanup;
mod pr_commands;
mod pr_rejection_fingerprint;
mod pr_review_handlers;
mod pr_review_watcher;
mod pr_undraft;
mod pr_watcher;
mod state;
pub(crate) mod tripwire_gate;
mod tripwire_hold;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pr_cleanup_tests;

#[cfg(test)]
mod tripwire_merge_gate_tests;

#[cfg(test)]
mod tripwire_gate_tests;

use crate::ci_preflight_gate::{
    CiPreflightGateKind, CiPreflightGateVerdict, latest_task_workdir,
    run_ci_reproduction_preflight_gate,
};
use crate::github_error_render::render_github_write_error;
use ci_helpers::{
    advisory_checks_section, blocking_failed_checks, build_ci_failure_sections, is_already_queued,
    is_failing_conclusion, is_merge_queue_405, parse_actions_run_id,
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
pub use pr_review_handlers::parse_pr_url;
use pr_undraft::{should_undraft_before_merge, warn_pr_merge_failed};
#[cfg(test)]
pub(crate) use pr_watcher::{
    PrDraftCiAction, decide_pr_draft_ci_action, rollout_policy_publication_marker,
};

use ci_failure_analysis::compute_ci_failure_fingerprint;
#[cfg(test)]
use ci_failure_analysis::{
    count_consecutive_identical, detect_scope_inversion, extract_crate_name, extract_crate_names,
};
#[cfg(test)]
use ci_helpers::is_advisory_check_name;
use ci_helpers::{CiMergeGateVerdict, ci_merge_gate_verdict};
#[cfg(test)]
use ci_helpers::{
    SameSignatureEscalationRoute, SameSignatureReproContext, build_generic_same_signature_reason,
    build_reproduction_plan_same_signature_reason, build_unreproducible_same_signature_reason,
    classify_same_signature_escalation,
};
#[cfg(test)]
use pr_commands::{dequeue_reason_is_failure, dequeue_requires_rework};
#[cfg(test)]
use pr_review_handlers::{
    is_racing_unmerged_status, pick_conflict_blocker_sibling,
    pr_transition_increments_reopen_count, record_pr_transition_reopen_metric,
};
