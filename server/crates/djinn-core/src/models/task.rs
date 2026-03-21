use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ── IssueType ─────────────────────────────────────────────────────────────────

/// All recognised task issue types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueType {
    /// Standard implementation task — full worker lifecycle with verification and review.
    Task,
    /// A product feature — same full lifecycle as `task`.
    Feature,
    /// A bug fix — same full lifecycle as `task`.
    Bug,
    /// Feasibility investigation — simple lifecycle (open → in_progress → closed).
    Spike,
    /// Open-ended research — simple lifecycle (open → in_progress → closed).
    Research,
    /// Epic/task decomposition planning — simple lifecycle, routed to Planner.
    Decomposition,
    /// Architecture/code review — simple lifecycle, routed to Architect.
    Review,
}

impl IssueType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Feature => "feature",
            Self::Bug => "bug",
            Self::Spike => "spike",
            Self::Research => "research",
            Self::Decomposition => "decomposition",
            Self::Review => "review",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "task" => Ok(Self::Task),
            "feature" => Ok(Self::Feature),
            "bug" => Ok(Self::Bug),
            "spike" => Ok(Self::Spike),
            "research" => Ok(Self::Research),
            "decomposition" => Ok(Self::Decomposition),
            "review" => Ok(Self::Review),
            other => Err(Error::Internal(format!("unknown issue_type: {other}"))),
        }
    }

    /// Returns `true` for types that use the simple lifecycle
    /// (open → in_progress → closed), skipping verification and review phases.
    pub fn uses_simple_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::Spike | Self::Research | Self::Decomposition | Self::Review
        )
    }
}

/// Task board work item, always scoped under an epic.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Task {
    pub id: String,
    pub project_id: String,
    pub short_id: String,
    pub epic_id: Option<String>,
    pub title: String,
    pub description: String,
    pub design: String,
    pub issue_type: String,
    pub status: String,
    pub priority: i64,
    pub owner: String,
    /// JSON array of label strings.
    pub labels: String,
    /// JSON array of acceptance-criteria objects.
    pub acceptance_criteria: String,
    pub reopen_count: i64,
    pub continuation_count: i64,
    pub verification_failure_count: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
    /// URL of the GitHub PR created when the GitHub App is connected.
    /// NULL when the direct-push merge path is used (no GitHub App).
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub pr_url: Option<String>,
    /// JSON metadata about an active merge conflict (set by conflict transitions
    /// and worktree rebase failures; cleared on submit_verification/close).
    pub merge_conflict_metadata: Option<String>,
    /// JSON array of memory note permalinks associated with this task.
    pub memory_refs: String,
    /// Specialist role name assigned to this task by the Planner (e.g. "rust-expert").
    /// When set, the slot lifecycle loads this Agent instead of the project default.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub agent_type: Option<String>,
    /// Number of unresolved blocker tasks (blocking tasks not yet closed).
    /// Populated by list queries via subquery; defaults to 0 elsewhere.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub unresolved_blocker_count: i64,
}

/// A single entry in the task activity log (audit trail + comments).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct ActivityEntry {
    pub id: String,
    pub task_id: Option<String>,
    pub actor_id: String,
    pub actor_role: String,
    pub event_type: String,
    /// JSON payload — shape varies by event_type.
    pub payload: String,
    pub created_at: String,
}

// ── State machine ─────────────────────────────────────────────────────────────

/// All valid task statuses. Serializes/deserializes to/from snake_case DB strings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Open,
    InProgress,
    Verifying,
    NeedsTaskReview,
    InTaskReview,
    /// PR has been opened and is awaiting CI/review/merge. Distinct from closed.
    PrReady,
    NeedsLeadIntervention,
    InLeadIntervention,
    Closed,
}

impl TaskStatus {
    /// The DB/wire string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Verifying => "verifying",
            Self::NeedsTaskReview => "needs_task_review",
            Self::InTaskReview => "in_task_review",
            Self::PrReady => "pr_ready",
            Self::NeedsLeadIntervention => "needs_lead_intervention",
            Self::InLeadIntervention => "in_lead_intervention",
            Self::Closed => "closed",
        }
    }

    /// Parse from a DB/wire string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(Self::Open),
            "in_progress" => Ok(Self::InProgress),
            "verifying" => Ok(Self::Verifying),
            "needs_task_review" => Ok(Self::NeedsTaskReview),
            "in_task_review" => Ok(Self::InTaskReview),
            "pr_ready" => Ok(Self::PrReady),
            "needs_lead_intervention" => Ok(Self::NeedsLeadIntervention),
            "in_lead_intervention" => Ok(Self::InLeadIntervention),
            "closed" => Ok(Self::Closed),
            other => Err(Error::Internal(format!("unknown task status: {other}"))),
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Named transition actions matching the MCP `task_transition` tool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionAction {
    Start,
    SubmitVerification,
    VerificationPass,
    VerificationFail,
    ReleaseVerification,
    SubmitTaskReview,
    TaskReviewStart,
    TaskReviewReject,
    /// Reviewer rejects with no AC progress — increments continuation_count.
    TaskReviewRejectStale,
    TaskReviewRejectConflict,
    TaskReviewApprove,
    Close,
    Reopen,
    Release,
    ReleaseTaskReview,
    ForceClose,
    UserOverride,
    /// System escalates stuck task to Lead intervention queue.
    Escalate,
    /// Lead agent starts working on an intervention task.
    LeadInterventionStart,
    /// Lead agent releases intervention (still needs attention).
    LeadInterventionRelease,
    /// Lead agent finishes intervention; task ready for worker again.
    LeadInterventionComplete,
    /// Lead agent approves implementation directly — triggers merge.
    LeadApprove,
    /// Merge conflict discovered during Lead approval — reopen for conflict resolver.
    LeadApproveConflict,
    /// Reviewer approves and opens a GitHub PR — transitions in_task_review → pr_ready.
    MarkPrReady,
    /// GitHub App signals PR merged — transitions pr_ready → closed.
    PrMerge,
    /// GitHub App signals changes requested on PR — transitions pr_ready → open.
    PrChangesRequested,
}

impl TransitionAction {
    /// Whether this action requires a non-empty `reason` string.
    pub fn requires_reason(&self) -> bool {
        matches!(
            self,
            Self::VerificationFail
                | Self::ReleaseVerification
                | Self::TaskReviewReject
                | Self::TaskReviewRejectStale
                | Self::TaskReviewRejectConflict
                | Self::Reopen
                | Self::Release
                | Self::ReleaseTaskReview
                | Self::ForceClose
                | Self::Escalate
                | Self::LeadInterventionRelease
                | Self::PrChangesRequested
        )
    }

    /// Parse from a wire string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "start" => Ok(Self::Start),
            "submit_verification" => Ok(Self::SubmitVerification),
            "verification_pass" => Ok(Self::VerificationPass),
            "verification_fail" => Ok(Self::VerificationFail),
            "release_verification" => Ok(Self::ReleaseVerification),
            "submit_task_review" => Ok(Self::SubmitTaskReview),
            "task_review_start" => Ok(Self::TaskReviewStart),
            "task_review_reject" => Ok(Self::TaskReviewReject),
            "task_review_reject_stale" => Ok(Self::TaskReviewRejectStale),
            "task_review_reject_conflict" => Ok(Self::TaskReviewRejectConflict),
            "task_review_approve" => Ok(Self::TaskReviewApprove),
            "close" => Ok(Self::Close),
            "reopen" => Ok(Self::Reopen),
            "release" => Ok(Self::Release),
            "release_task_review" => Ok(Self::ReleaseTaskReview),
            "force_close" => Ok(Self::ForceClose),
            "user_override" => Ok(Self::UserOverride),
            "escalate" => Ok(Self::Escalate),
            "lead_intervention_start" => Ok(Self::LeadInterventionStart),
            "lead_intervention_release" => Ok(Self::LeadInterventionRelease),
            "lead_intervention_complete" => Ok(Self::LeadInterventionComplete),
            "lead_approve" => Ok(Self::LeadApprove),
            "lead_approve_conflict" => Ok(Self::LeadApproveConflict),
            "mark_pr_ready" => Ok(Self::MarkPrReady),
            "pr_merge" => Ok(Self::PrMerge),
            "pr_changes_requested" => Ok(Self::PrChangesRequested),
            other => Err(Error::Internal(format!(
                "unknown transition action: {other}"
            ))),
        }
    }
}

/// The computed effect of a validated transition.
///
/// Returned by [`compute_transition`]; applied atomically by `TaskRepository::transition`.
pub struct TransitionApply {
    /// Target status.
    pub to_status: Option<TaskStatus>,
    /// Increment `reopen_count` by 1.
    pub increment_reopen: bool,
    /// Reset `continuation_count` to 0.
    pub reset_continuation: bool,
    /// Increment `continuation_count` by 1 (for stale reopen detection).
    pub increment_continuation: bool,
    /// Increment `verification_failure_count` by 1.
    pub increment_verification_failure: bool,
    /// Reset `verification_failure_count` to 0.
    pub reset_verification_failure: bool,
    /// Set `closed_at` to the current timestamp.
    pub set_closed_at: bool,
    /// Set `closed_at` to NULL.
    pub clear_closed_at: bool,
    /// Set `close_reason` to this value.
    pub close_reason: Option<&'static str>,
    /// Set `close_reason` to NULL.
    pub clear_close_reason: bool,
    /// Set merge_conflict_metadata to a value (caller provides the JSON).
    pub set_merge_conflict_metadata: bool,
    /// Clear merge_conflict_metadata to NULL.
    pub clear_merge_conflict_metadata: bool,
    /// Value for `event_type` in the activity log entry.
    pub activity_type: &'static str,
}

impl Default for TransitionApply {
    fn default() -> Self {
        Self {
            to_status: None,
            increment_reopen: false,
            reset_continuation: false,
            increment_continuation: false,
            increment_verification_failure: false,
            reset_verification_failure: false,
            set_closed_at: false,
            clear_closed_at: false,
            close_reason: None,
            clear_close_reason: false,
            set_merge_conflict_metadata: false,
            clear_merge_conflict_metadata: false,
            activity_type: "status_changed",
        }
    }
}

impl TransitionApply {
    fn simple(to: TaskStatus) -> Self {
        Self {
            to_status: Some(to),
            ..Default::default()
        }
    }
}

/// Validate a transition and return the set of effects to apply.
///
/// Does **not** check unresolved blockers — the caller handles that for `Start`.
/// Does **not** touch the database.
pub fn compute_transition(
    action: &TransitionAction,
    from: &TaskStatus,
    target_override: Option<&TaskStatus>,
) -> Result<TransitionApply> {
    let bad = |msg: &str| Err(Error::InvalidTransition(msg.to_owned()));

    Ok(match action {
        TransitionAction::Start => {
            if *from != TaskStatus::Open {
                return bad("start is only valid from open");
            }
            TransitionApply::simple(TaskStatus::InProgress)
        }

        TransitionAction::SubmitVerification => {
            if *from != TaskStatus::InProgress {
                return bad("submit_verification is only valid from in_progress");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Verifying),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::VerificationPass => {
            if *from != TaskStatus::Verifying {
                return bad("verification_pass is only valid from verifying");
            }
            TransitionApply {
                to_status: Some(TaskStatus::NeedsTaskReview),
                reset_verification_failure: true,
                ..Default::default()
            }
        }

        TransitionAction::VerificationFail => {
            if *from != TaskStatus::Verifying {
                return bad("verification_fail is only valid from verifying");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_verification_failure: true,
                ..Default::default()
            }
        }

        TransitionAction::ReleaseVerification => {
            if *from != TaskStatus::Verifying {
                return bad("release_verification is only valid from verifying");
            }
            TransitionApply::simple(TaskStatus::Open)
        }

        TransitionAction::SubmitTaskReview => {
            if !matches!(from, TaskStatus::InProgress | TaskStatus::Verifying) {
                return bad("submit_task_review is only valid from in_progress or verifying");
            }
            TransitionApply {
                to_status: Some(TaskStatus::NeedsTaskReview),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewStart => {
            if *from != TaskStatus::NeedsTaskReview {
                return bad("task_review_start is only valid from needs_task_review");
            }
            TransitionApply::simple(TaskStatus::InTaskReview)
        }

        TransitionAction::TaskReviewReject => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_reject is only valid from in_task_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                // continuation_count handled by circuit breaker (reset if progress, increment if stale)
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewRejectStale => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_reject_stale is only valid from in_task_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                increment_continuation: true,
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewRejectConflict => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_reject_conflict is only valid from in_task_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                set_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewApprove => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_approve is only valid from in_task_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some("completed"),
                ..Default::default()
            }
        }

        TransitionAction::Close => {
            if *from == TaskStatus::Closed {
                return bad("task is already closed");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some("completed"),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::Reopen => {
            if *from != TaskStatus::Closed {
                return bad("reopen is only valid from closed");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                reset_continuation: true,
                clear_closed_at: true,
                clear_close_reason: true,
                ..Default::default()
            }
        }

        TransitionAction::Release => {
            if *from != TaskStatus::InProgress {
                return bad("release is only valid from in_progress");
            }
            TransitionApply::simple(TaskStatus::Open)
        }

        TransitionAction::ReleaseTaskReview => {
            if *from != TaskStatus::InTaskReview {
                return bad("release_task_review is only valid from in_task_review");
            }
            TransitionApply::simple(TaskStatus::NeedsTaskReview)
        }

        TransitionAction::ForceClose => {
            if *from == TaskStatus::Closed {
                return bad("task is already closed");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some("force_closed"),
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::UserOverride => {
            let target = target_override.ok_or_else(|| {
                Error::InvalidTransition("user_override requires target_status".to_owned())
            })?;
            let closing = *target == TaskStatus::Closed;
            TransitionApply {
                to_status: Some(target.clone()),
                reset_continuation: true,
                set_closed_at: closing,
                clear_closed_at: !closing,
                close_reason: if closing { Some("force_closed") } else { None },
                clear_close_reason: !closing,
                clear_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::Escalate => {
            if !matches!(
                from,
                TaskStatus::Open
                    | TaskStatus::InProgress
                    | TaskStatus::InTaskReview
                    | TaskStatus::Verifying
            ) {
                return bad(
                    "escalate is only valid from open, in_progress, in_task_review, or verifying",
                );
            }
            TransitionApply {
                to_status: Some(TaskStatus::NeedsLeadIntervention),
                reset_continuation: true,
                ..Default::default()
            }
        }

        TransitionAction::LeadInterventionStart => {
            if *from != TaskStatus::NeedsLeadIntervention {
                return bad("lead_intervention_start is only valid from needs_lead_intervention");
            }
            TransitionApply::simple(TaskStatus::InLeadIntervention)
        }

        TransitionAction::LeadInterventionRelease => {
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_intervention_release is only valid from in_lead_intervention");
            }
            TransitionApply::simple(TaskStatus::NeedsLeadIntervention)
        }

        TransitionAction::LeadInterventionComplete => {
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_intervention_complete is only valid from in_lead_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                reset_verification_failure: true,
                ..Default::default()
            }
        }

        TransitionAction::LeadApprove => {
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_approve is only valid from in_lead_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some("completed"),
                ..Default::default()
            }
        }

        TransitionAction::LeadApproveConflict => {
            if *from != TaskStatus::InLeadIntervention {
                return bad("lead_approve_conflict is only valid from in_lead_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                set_merge_conflict_metadata: true,
                ..Default::default()
            }
        }

        TransitionAction::MarkPrReady => {
            if !matches!(
                from,
                TaskStatus::InTaskReview | TaskStatus::InLeadIntervention
            ) {
                return bad(
                    "mark_pr_ready is only valid from in_task_review or in_lead_intervention",
                );
            }
            TransitionApply::simple(TaskStatus::PrReady)
        }

        TransitionAction::PrMerge => {
            if *from != TaskStatus::PrReady {
                return bad("pr_merge is only valid from pr_ready");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some("completed"),
                ..Default::default()
            }
        }

        TransitionAction::PrChangesRequested => {
            if *from != TaskStatus::PrReady {
                return bad("pr_changes_requested is only valid from pr_ready");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                ..Default::default()
            }
        }
    })
}

/// Validate a transition, routing to the appropriate lifecycle based on `issue_type`.
///
/// For `spike`, `research`, `decomposition`, and `review` task types the simple
/// lifecycle applies: `open → in_progress → closed`.  Actions that belong only to
/// the full worker lifecycle (submit_verification, verification_*, task_review_*,
/// lead_intervention_*) are rejected for these types.
///
/// All other issue types (task, feature, bug) use the full lifecycle via
/// [`compute_transition`].
pub fn compute_transition_for_issue_type(
    action: &TransitionAction,
    from: &TaskStatus,
    target_override: Option<&TaskStatus>,
    issue_type: &str,
) -> Result<TransitionApply> {
    let uses_simple = IssueType::parse(issue_type)
        .map(|it| it.uses_simple_lifecycle())
        .unwrap_or(false);

    if uses_simple {
        // Restrict to actions that make sense in the simple lifecycle.
        let allowed = matches!(
            action,
            TransitionAction::Start
                | TransitionAction::Close
                | TransitionAction::ForceClose
                | TransitionAction::Reopen
                | TransitionAction::Release
                | TransitionAction::UserOverride
                | TransitionAction::Escalate
                | TransitionAction::LeadInterventionStart
                | TransitionAction::LeadInterventionRelease
                | TransitionAction::LeadInterventionComplete
                | TransitionAction::LeadApprove
                | TransitionAction::LeadApproveConflict
        );
        if !allowed {
            return Err(Error::InvalidTransition(format!(
                "action {action:?} is not valid for issue_type '{issue_type}' (simple lifecycle: open → in_progress → closed)"
            )));
        }
    }

    compute_transition(action, from, target_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUSES: [TaskStatus; 9] = [
        TaskStatus::Open,
        TaskStatus::InProgress,
        TaskStatus::Verifying,
        TaskStatus::NeedsTaskReview,
        TaskStatus::InTaskReview,
        TaskStatus::PrReady,
        TaskStatus::NeedsLeadIntervention,
        TaskStatus::InLeadIntervention,
        TaskStatus::Closed,
    ];

    const ACTIONS: [TransitionAction; 26] = [
        TransitionAction::Start,
        TransitionAction::SubmitVerification,
        TransitionAction::VerificationPass,
        TransitionAction::VerificationFail,
        TransitionAction::ReleaseVerification,
        TransitionAction::SubmitTaskReview,
        TransitionAction::TaskReviewStart,
        TransitionAction::TaskReviewReject,
        TransitionAction::TaskReviewRejectStale,
        TransitionAction::TaskReviewRejectConflict,
        TransitionAction::TaskReviewApprove,
        TransitionAction::Close,
        TransitionAction::Reopen,
        TransitionAction::Release,
        TransitionAction::ReleaseTaskReview,
        TransitionAction::ForceClose,
        TransitionAction::UserOverride,
        TransitionAction::Escalate,
        TransitionAction::LeadInterventionStart,
        TransitionAction::LeadInterventionRelease,
        TransitionAction::LeadInterventionComplete,
        TransitionAction::LeadApprove,
        TransitionAction::LeadApproveConflict,
        TransitionAction::MarkPrReady,
        TransitionAction::PrMerge,
        TransitionAction::PrChangesRequested,
    ];

    fn expected_status(action: &TransitionAction, from: &TaskStatus) -> Option<TaskStatus> {
        match (action, from) {
            (TransitionAction::Start, TaskStatus::Open) => Some(TaskStatus::InProgress),
            (TransitionAction::SubmitVerification, TaskStatus::InProgress) => {
                Some(TaskStatus::Verifying)
            }
            (TransitionAction::VerificationPass, TaskStatus::Verifying) => {
                Some(TaskStatus::NeedsTaskReview)
            }
            (TransitionAction::VerificationFail, TaskStatus::Verifying) => Some(TaskStatus::Open),
            (TransitionAction::ReleaseVerification, TaskStatus::Verifying) => {
                Some(TaskStatus::Open)
            }
            (
                TransitionAction::SubmitTaskReview,
                TaskStatus::InProgress | TaskStatus::Verifying,
            ) => Some(TaskStatus::NeedsTaskReview),
            (TransitionAction::TaskReviewStart, TaskStatus::NeedsTaskReview) => {
                Some(TaskStatus::InTaskReview)
            }
            (TransitionAction::TaskReviewReject, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::TaskReviewRejectStale, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::TaskReviewRejectConflict, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::TaskReviewApprove, TaskStatus::InTaskReview) => {
                Some(TaskStatus::Closed)
            }
            (TransitionAction::Close, s) if *s != TaskStatus::Closed => Some(TaskStatus::Closed),
            (TransitionAction::Reopen, TaskStatus::Closed) => Some(TaskStatus::Open),
            (TransitionAction::Release, TaskStatus::InProgress) => Some(TaskStatus::Open),
            (TransitionAction::ReleaseTaskReview, TaskStatus::InTaskReview) => {
                Some(TaskStatus::NeedsTaskReview)
            }
            (TransitionAction::ForceClose, s) if *s != TaskStatus::Closed => {
                Some(TaskStatus::Closed)
            }
            (
                TransitionAction::Escalate,
                TaskStatus::Open
                | TaskStatus::InProgress
                | TaskStatus::InTaskReview
                | TaskStatus::Verifying,
            ) => Some(TaskStatus::NeedsLeadIntervention),
            (TransitionAction::LeadInterventionStart, TaskStatus::NeedsLeadIntervention) => {
                Some(TaskStatus::InLeadIntervention)
            }
            (TransitionAction::LeadInterventionRelease, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::NeedsLeadIntervention)
            }
            (TransitionAction::LeadInterventionComplete, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::LeadApprove, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Closed)
            }
            (TransitionAction::LeadApproveConflict, TaskStatus::InLeadIntervention) => {
                Some(TaskStatus::Open)
            }
            (
                TransitionAction::MarkPrReady,
                TaskStatus::InTaskReview | TaskStatus::InLeadIntervention,
            ) => Some(TaskStatus::PrReady),
            (TransitionAction::PrMerge, TaskStatus::PrReady) => Some(TaskStatus::Closed),
            (TransitionAction::PrChangesRequested, TaskStatus::PrReady) => Some(TaskStatus::Open),
            _ => None,
        }
    }

    #[test]
    fn transition_matrix_valid_and_invalid_pairs() {
        for action in ACTIONS {
            for from in &STATUSES {
                if matches!(action, TransitionAction::UserOverride) {
                    continue;
                }
                let res = compute_transition(&action, from, None);
                match expected_status(&action, from) {
                    Some(to) => {
                        let apply = res.unwrap_or_else(|_| {
                            panic!("expected valid {:?} from {:?}", action, from)
                        });
                        assert_eq!(
                            apply.to_status,
                            Some(to),
                            "wrong to_status for {:?} from {:?}",
                            action,
                            from
                        );
                    }
                    None => {
                        assert!(
                            res.is_err(),
                            "expected invalid {:?} from {:?}",
                            action,
                            from
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn user_override_requires_target_and_applies_target() {
        assert!(
            compute_transition(&TransitionAction::UserOverride, &TaskStatus::Open, None).is_err()
        );

        let closed = compute_transition(
            &TransitionAction::UserOverride,
            &TaskStatus::InProgress,
            Some(&TaskStatus::Closed),
        )
        .expect("closed override should be valid");
        assert_eq!(closed.to_status, Some(TaskStatus::Closed));
        assert!(closed.set_closed_at);
        assert_eq!(closed.close_reason, Some("force_closed"));
        assert!(!closed.clear_close_reason);

        let open = compute_transition(
            &TransitionAction::UserOverride,
            &TaskStatus::Closed,
            Some(&TaskStatus::Open),
        )
        .expect("open override should be valid");
        assert_eq!(open.to_status, Some(TaskStatus::Open));
        assert!(open.clear_closed_at);
        assert!(open.clear_close_reason);
    }

    #[test]
    fn continuation_escalation_threshold_behavior() {
        let stale = compute_transition(
            &TransitionAction::TaskReviewRejectStale,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("stale reject should be valid");
        assert!(stale.increment_continuation);
        assert!(stale.increment_reopen);

        let escalate = compute_transition(&TransitionAction::Escalate, &TaskStatus::Open, None)
            .expect("escalate should be valid from open");
        assert_eq!(escalate.to_status, Some(TaskStatus::NeedsLeadIntervention));
        assert!(escalate.reset_continuation);
    }

    #[test]
    fn stale_rejections_three_cycles_trigger_lead_intervention_at_threshold() {
        let mut status = TaskStatus::InTaskReview;
        let mut continuation_count = 0;

        for _cycle in 1..=3 {
            let stale = compute_transition(&TransitionAction::TaskReviewRejectStale, &status, None)
                .expect("stale reject should be valid from in_task_review");
            assert_eq!(stale.to_status, Some(TaskStatus::Open));
            assert!(stale.increment_continuation);
            continuation_count += 1;
            status = stale.to_status.expect("stale reject should set status");

            if continuation_count >= 3 {
                let escalate = compute_transition(&TransitionAction::Escalate, &status, None)
                    .expect("threshold stale count should allow escalation from open");
                assert_eq!(escalate.to_status, Some(TaskStatus::NeedsLeadIntervention));
                assert!(escalate.reset_continuation);
                status = escalate.to_status.expect("escalate should set status");
                assert_eq!(status, TaskStatus::NeedsLeadIntervention);
            } else {
                let start = compute_transition(&TransitionAction::Start, &status, None)
                    .expect("open should start");
                assert_eq!(start.to_status, Some(TaskStatus::InProgress));
                status = start.to_status.expect("start should set status");

                let submit = compute_transition(&TransitionAction::SubmitTaskReview, &status, None)
                    .expect("in_progress should submit to task review");
                assert_eq!(submit.to_status, Some(TaskStatus::NeedsTaskReview));
                status = submit.to_status.expect("submit should set status");

                let review_start =
                    compute_transition(&TransitionAction::TaskReviewStart, &status, None)
                        .expect("needs_task_review should enter in_task_review");
                assert_eq!(review_start.to_status, Some(TaskStatus::InTaskReview));
                status = review_start
                    .to_status
                    .expect("task_review_start should set status");
            }
        }
    }

    #[test]
    fn met_snapshot_stale_detection_actions_are_distinct() {
        let stale = compute_transition(
            &TransitionAction::TaskReviewRejectStale,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("stale reject should be valid");
        assert!(stale.increment_continuation);
        assert!(!stale.reset_continuation);

        let progress = compute_transition(
            &TransitionAction::TaskReviewReject,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("regular reject should be valid");
        assert!(!progress.increment_continuation);
        assert!(!progress.reset_continuation);

        let conflict = compute_transition(
            &TransitionAction::TaskReviewRejectConflict,
            &TaskStatus::InTaskReview,
            None,
        )
        .expect("conflict reject should be valid");
        assert!(!conflict.increment_continuation);
        assert!(conflict.reset_continuation);
    }

    #[test]
    fn conflict_metadata_flags_set_and_cleared() {
        // Conflict transitions set the flag
        let conflict_reject = compute_transition(
            &TransitionAction::TaskReviewRejectConflict,
            &TaskStatus::InTaskReview,
            None,
        )
        .unwrap();
        assert!(conflict_reject.set_merge_conflict_metadata);
        assert!(!conflict_reject.clear_merge_conflict_metadata);

        let pm_conflict = compute_transition(
            &TransitionAction::LeadApproveConflict,
            &TaskStatus::InLeadIntervention,
            None,
        )
        .unwrap();
        assert!(pm_conflict.set_merge_conflict_metadata);
        assert!(!pm_conflict.clear_merge_conflict_metadata);

        // Clearing transitions
        let submit_verify = compute_transition(
            &TransitionAction::SubmitVerification,
            &TaskStatus::InProgress,
            None,
        )
        .unwrap();
        assert!(submit_verify.clear_merge_conflict_metadata);
        assert!(!submit_verify.set_merge_conflict_metadata);

        let submit_review = compute_transition(
            &TransitionAction::SubmitTaskReview,
            &TaskStatus::InProgress,
            None,
        )
        .unwrap();
        assert!(submit_review.clear_merge_conflict_metadata);

        let close = compute_transition(&TransitionAction::Close, &TaskStatus::Open, None).unwrap();
        assert!(close.clear_merge_conflict_metadata);

        let force_close =
            compute_transition(&TransitionAction::ForceClose, &TaskStatus::Open, None).unwrap();
        assert!(force_close.clear_merge_conflict_metadata);

        let user_override = compute_transition(
            &TransitionAction::UserOverride,
            &TaskStatus::InProgress,
            Some(&TaskStatus::Open),
        )
        .unwrap();
        assert!(user_override.clear_merge_conflict_metadata);

        // Start does NOT clear
        let start = compute_transition(&TransitionAction::Start, &TaskStatus::Open, None).unwrap();
        assert!(!start.clear_merge_conflict_metadata);
        assert!(!start.set_merge_conflict_metadata);
    }
}
