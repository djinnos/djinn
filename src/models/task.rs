use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Task board work item, always scoped under an epic.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
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
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub blocked_from_status: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
    /// JSON array of memory note permalinks associated with this task.
    pub memory_refs: String,
}

/// A single entry in the task activity log (audit trail + comments).
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
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
    Draft,
    Open,
    InProgress,
    NeedsTaskReview,
    InTaskReview,
    NeedsEpicReview,
    InEpicReview,
    Approved,
    Closed,
    Blocked,
}

impl TaskStatus {
    /// The DB/wire string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::NeedsTaskReview => "needs_task_review",
            Self::InTaskReview => "in_task_review",
            Self::NeedsEpicReview => "needs_epic_review",
            Self::InEpicReview => "in_epic_review",
            Self::Approved => "approved",
            Self::Closed => "closed",
            Self::Blocked => "blocked",
        }
    }

    /// Parse from a DB/wire string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "draft" => Ok(Self::Draft),
            "open" => Ok(Self::Open),
            "in_progress" => Ok(Self::InProgress),
            "needs_task_review" => Ok(Self::NeedsTaskReview),
            "in_task_review" => Ok(Self::InTaskReview),
            "needs_epic_review" => Ok(Self::NeedsEpicReview),
            "in_epic_review" => Ok(Self::InEpicReview),
            "approved" => Ok(Self::Approved),
            "closed" => Ok(Self::Closed),
            "blocked" => Ok(Self::Blocked),
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
    Accept,
    Start,
    SubmitTaskReview,
    TaskReviewStart,
    TaskReviewReject,
    TaskReviewRejectConflict,
    TaskReviewApprove,
    EpicReviewStart,
    EpicReviewReject,
    EpicReviewApprove,
    Close,
    Reopen,
    Release,
    ReleaseTaskReview,
    ReleaseEpicReview,
    Block,
    Unblock,
    ForceClose,
    UserOverride,
}

impl TransitionAction {
    /// Whether this action requires a non-empty `reason` string.
    pub fn requires_reason(&self) -> bool {
        matches!(
            self,
            Self::TaskReviewReject
                | Self::TaskReviewRejectConflict
                | Self::EpicReviewReject
                | Self::Reopen
                | Self::Release
                | Self::ReleaseTaskReview
                | Self::ReleaseEpicReview
                | Self::Block
                | Self::ForceClose
        )
    }

    /// Parse from a wire string.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "accept" => Ok(Self::Accept),
            "start" => Ok(Self::Start),
            "submit_task_review" => Ok(Self::SubmitTaskReview),
            "task_review_start" => Ok(Self::TaskReviewStart),
            "task_review_reject" => Ok(Self::TaskReviewReject),
            "task_review_reject_conflict" => Ok(Self::TaskReviewRejectConflict),
            "task_review_approve" => Ok(Self::TaskReviewApprove),
            "epic_review_start" => Ok(Self::EpicReviewStart),
            "epic_review_reject" => Ok(Self::EpicReviewReject),
            "epic_review_approve" => Ok(Self::EpicReviewApprove),
            "close" => Ok(Self::Close),
            "reopen" => Ok(Self::Reopen),
            "release" => Ok(Self::Release),
            "release_task_review" => Ok(Self::ReleaseTaskReview),
            "release_epic_review" => Ok(Self::ReleaseEpicReview),
            "block" => Ok(Self::Block),
            "unblock" => Ok(Self::Unblock),
            "force_close" => Ok(Self::ForceClose),
            "user_override" => Ok(Self::UserOverride),
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
    /// Target status. `None` means restore from `blocked_from_status` (Unblock only).
    pub to_status: Option<TaskStatus>,
    /// Increment `reopen_count` by 1.
    pub increment_reopen: bool,
    /// Reset `continuation_count` to 0.
    pub reset_continuation: bool,
    /// Set `closed_at` to the current timestamp.
    pub set_closed_at: bool,
    /// Set `closed_at` to NULL.
    pub clear_closed_at: bool,
    /// Store the current status as `blocked_from_status` (Block action).
    pub save_blocked_from: bool,
    /// Set `blocked_from_status` to NULL.
    pub clear_blocked_from: bool,
    /// Set `close_reason` to this value.
    pub close_reason: Option<&'static str>,
    /// Set `close_reason` to NULL.
    pub clear_close_reason: bool,
    /// Value for `event_type` in the activity log entry.
    pub activity_type: &'static str,
}

impl Default for TransitionApply {
    fn default() -> Self {
        Self {
            to_status: None,
            increment_reopen: false,
            reset_continuation: false,
            set_closed_at: false,
            clear_closed_at: false,
            save_blocked_from: false,
            clear_blocked_from: false,
            close_reason: None,
            clear_close_reason: false,
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
        TransitionAction::Accept => {
            if *from != TaskStatus::Draft {
                return bad("accept is only valid from draft");
            }
            TransitionApply::simple(TaskStatus::Open)
        }

        TransitionAction::Start => {
            if *from != TaskStatus::Open {
                return bad("start is only valid from open");
            }
            TransitionApply::simple(TaskStatus::InProgress)
        }

        TransitionAction::SubmitTaskReview => {
            if *from != TaskStatus::InProgress {
                return bad("submit_task_review is only valid from in_progress");
            }
            TransitionApply::simple(TaskStatus::NeedsTaskReview)
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
                reset_continuation: true,
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
                ..Default::default()
            }
        }

        TransitionAction::TaskReviewApprove => {
            if *from != TaskStatus::InTaskReview {
                return bad("task_review_approve is only valid from in_task_review");
            }
            TransitionApply::simple(TaskStatus::NeedsEpicReview)
        }

        TransitionAction::EpicReviewStart => {
            if *from != TaskStatus::NeedsEpicReview {
                return bad("epic_review_start is only valid from needs_epic_review");
            }
            TransitionApply::simple(TaskStatus::InEpicReview)
        }

        TransitionAction::EpicReviewReject => {
            if *from != TaskStatus::InEpicReview {
                return bad("epic_review_reject is only valid from in_epic_review");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                increment_reopen: true,
                reset_continuation: true,
                ..Default::default()
            }
        }

        TransitionAction::EpicReviewApprove => {
            if *from != TaskStatus::InEpicReview {
                return bad("epic_review_approve is only valid from in_epic_review");
            }
            TransitionApply::simple(TaskStatus::Approved)
        }

        TransitionAction::Close => {
            if *from != TaskStatus::Approved {
                return bad("close is only valid from approved");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                close_reason: Some("completed"),
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
            if *from != TaskStatus::InProgress && *from != TaskStatus::Blocked {
                return bad("release is only valid from in_progress or blocked");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Open),
                clear_blocked_from: *from == TaskStatus::Blocked,
                ..Default::default()
            }
        }

        TransitionAction::ReleaseTaskReview => {
            if *from != TaskStatus::InTaskReview {
                return bad("release_task_review is only valid from in_task_review");
            }
            TransitionApply::simple(TaskStatus::NeedsTaskReview)
        }

        TransitionAction::ReleaseEpicReview => {
            if *from != TaskStatus::InEpicReview {
                return bad("release_epic_review is only valid from in_epic_review");
            }
            TransitionApply::simple(TaskStatus::NeedsEpicReview)
        }

        TransitionAction::Block => {
            if *from == TaskStatus::Closed {
                return bad("cannot block a closed task");
            }
            if *from == TaskStatus::Blocked {
                return bad("task is already blocked");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Blocked),
                save_blocked_from: true,
                activity_type: "blocked",
                ..Default::default()
            }
        }

        TransitionAction::Unblock => {
            if *from != TaskStatus::Blocked {
                return bad("unblock is only valid from blocked");
            }
            TransitionApply {
                to_status: None, // resolved by caller from blocked_from_status
                clear_blocked_from: true,
                reset_continuation: true,
                activity_type: "unblocked",
                ..Default::default()
            }
        }

        TransitionAction::ForceClose => {
            if *from == TaskStatus::Closed {
                return bad("task is already closed");
            }
            TransitionApply {
                to_status: Some(TaskStatus::Closed),
                set_closed_at: true,
                clear_blocked_from: true,
                close_reason: Some("force_closed"),
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
                clear_blocked_from: true,
                close_reason: if closing { Some("force_closed") } else { None },
                clear_close_reason: !closing,
                ..Default::default()
            }
        }
    })
}
