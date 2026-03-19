use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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
    /// JSON metadata about an active merge conflict (set by conflict transitions
    /// and worktree rebase failures; cleared on submit_verification/close).
    pub merge_conflict_metadata: Option<String>,
    /// JSON array of memory note permalinks associated with this task.
    pub memory_refs: String,
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
    NeedsPmIntervention,
    InPmIntervention,
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
            Self::NeedsPmIntervention => "needs_pm_intervention",
            Self::InPmIntervention => "in_pm_intervention",
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
            "needs_pm_intervention" => Ok(Self::NeedsPmIntervention),
            "in_pm_intervention" => Ok(Self::InPmIntervention),
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
    /// System escalates stuck task to PM intervention queue.
    Escalate,
    /// PM agent starts working on an intervention task.
    PmInterventionStart,
    /// PM agent releases intervention (still needs attention).
    PmInterventionRelease,
    /// PM agent finishes intervention; task ready for worker again.
    PmInterventionComplete,
    /// PM agent approves implementation directly — triggers merge.
    PmApprove,
    /// Merge conflict discovered during PM approval — reopen for conflict resolver.
    PmApproveConflict,
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
                | Self::PmInterventionRelease
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
            "pm_intervention_start" => Ok(Self::PmInterventionStart),
            "pm_intervention_release" => Ok(Self::PmInterventionRelease),
            "pm_intervention_complete" => Ok(Self::PmInterventionComplete),
            "pm_approve" => Ok(Self::PmApprove),
            "pm_approve_conflict" => Ok(Self::PmApproveConflict),
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
                to_status: Some(TaskStatus::NeedsPmIntervention),
                reset_continuation: true,
                ..Default::default()
            }
        }

        TransitionAction::PmInterventionStart => {
            if *from != TaskStatus::NeedsPmIntervention {
                return bad("pm_intervention_start is only valid from needs_pm_intervention");
            }
            TransitionApply::simple(TaskStatus::InPmIntervention)
        }

        TransitionAction::PmInterventionRelease => {
            if *from != TaskStatus::InPmIntervention {
                return bad("pm_intervention_release is only valid from in_pm_intervention");
            }
            TransitionApply::simple(TaskStatus::NeedsPmIntervention)
        }

        TransitionAction::PmInterventionComplete => {
            if *from != TaskStatus::InPmIntervention {
                return bad("pm_intervention_complete is only valid from in_pm_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                reset_verification_failure: true,
                ..Default::default()
            }
        }

        TransitionAction::PmApprove => {
            if *from != TaskStatus::InPmIntervention {
                return bad("pm_approve is only valid from in_pm_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some("completed"),
                ..Default::default()
            }
        }

        TransitionAction::PmApproveConflict => {
            if *from != TaskStatus::InPmIntervention {
                return bad("pm_approve_conflict is only valid from in_pm_intervention");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                reset_continuation: true,
                set_merge_conflict_metadata: true,
                ..Default::default()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUSES: [TaskStatus; 8] = [
        TaskStatus::Open,
        TaskStatus::InProgress,
        TaskStatus::Verifying,
        TaskStatus::NeedsTaskReview,
        TaskStatus::InTaskReview,
        TaskStatus::NeedsPmIntervention,
        TaskStatus::InPmIntervention,
        TaskStatus::Closed,
    ];

    const ACTIONS: [TransitionAction; 23] = [
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
        TransitionAction::PmInterventionStart,
        TransitionAction::PmInterventionRelease,
        TransitionAction::PmInterventionComplete,
        TransitionAction::PmApprove,
        TransitionAction::PmApproveConflict,
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
            ) => Some(TaskStatus::NeedsPmIntervention),
            (TransitionAction::PmInterventionStart, TaskStatus::NeedsPmIntervention) => {
                Some(TaskStatus::InPmIntervention)
            }
            (TransitionAction::PmInterventionRelease, TaskStatus::InPmIntervention) => {
                Some(TaskStatus::NeedsPmIntervention)
            }
            (TransitionAction::PmInterventionComplete, TaskStatus::InPmIntervention) => {
                Some(TaskStatus::Open)
            }
            (TransitionAction::PmApprove, TaskStatus::InPmIntervention) => Some(TaskStatus::Closed),
            (TransitionAction::PmApproveConflict, TaskStatus::InPmIntervention) => {
                Some(TaskStatus::Open)
            }
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
        assert_eq!(escalate.to_status, Some(TaskStatus::NeedsPmIntervention));
        assert!(escalate.reset_continuation);
    }

    #[test]
    fn stale_rejections_three_cycles_trigger_pm_intervention_at_threshold() {
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
                assert_eq!(escalate.to_status, Some(TaskStatus::NeedsPmIntervention));
                assert!(escalate.reset_continuation);
                status = escalate.to_status.expect("escalate should set status");
                assert_eq!(status, TaskStatus::NeedsPmIntervention);
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
        ).unwrap();
        assert!(conflict_reject.set_merge_conflict_metadata);
        assert!(!conflict_reject.clear_merge_conflict_metadata);

        let pm_conflict = compute_transition(
            &TransitionAction::PmApproveConflict,
            &TaskStatus::InPmIntervention,
            None,
        ).unwrap();
        assert!(pm_conflict.set_merge_conflict_metadata);
        assert!(!pm_conflict.clear_merge_conflict_metadata);

        // Clearing transitions
        let submit_verify = compute_transition(
            &TransitionAction::SubmitVerification,
            &TaskStatus::InProgress,
            None,
        ).unwrap();
        assert!(submit_verify.clear_merge_conflict_metadata);
        assert!(!submit_verify.set_merge_conflict_metadata);

        let submit_review = compute_transition(
            &TransitionAction::SubmitTaskReview,
            &TaskStatus::InProgress,
            None,
        ).unwrap();
        assert!(submit_review.clear_merge_conflict_metadata);

        let close = compute_transition(
            &TransitionAction::Close,
            &TaskStatus::Open,
            None,
        ).unwrap();
        assert!(close.clear_merge_conflict_metadata);

        let force_close = compute_transition(
            &TransitionAction::ForceClose,
            &TaskStatus::Open,
            None,
        ).unwrap();
        assert!(force_close.clear_merge_conflict_metadata);

        let user_override = compute_transition(
            &TransitionAction::UserOverride,
            &TaskStatus::InProgress,
            Some(&TaskStatus::Open),
        ).unwrap();
        assert!(user_override.clear_merge_conflict_metadata);

        // Start does NOT clear
        let start = compute_transition(
            &TransitionAction::Start,
            &TaskStatus::Open,
            None,
        ).unwrap();
        assert!(!start.clear_merge_conflict_metadata);
        assert!(!start.set_merge_conflict_metadata);
    }
}
