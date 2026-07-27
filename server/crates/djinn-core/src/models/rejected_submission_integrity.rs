use std::fmt;

use serde::{Deserialize, Serialize};

/// Origin label of a rejected submission verdict.
///
/// Wire strings are the lowercase variant names stored as VARCHAR(64). Kept as
/// a small, known set of origins so the live submit-work guard can reason
/// about why a fingerprint was recorded as rejected; unknown labels parse to
/// [`RejectedVerdictKind::Other`] rather than erroring, so new verdict
/// origins can be added without coordinating enum updates.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RejectedVerdictKind {
    /// No-progress streak crossed the configured threshold.
    #[default]
    NoProgress,
    /// A reviewer explicitly rejected the submission.
    ReviewerReject,
    /// The model was detected looping over identical turns.
    Looping,
    /// A soft deadline fired and the submission was rejected.
    SoftDeadline,
    /// Any other / forward-runtime verdict label.
    Other,
}

impl RejectedVerdictKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoProgress => "no_progress",
            Self::ReviewerReject => "reviewer_reject",
            Self::Looping => "looping",
            Self::SoftDeadline => "soft_deadline",
            Self::Other => "other",
        }
    }

    /// Parse a verdict label into a known variant, falling back to
    /// [`Self::Other`] for unrecognized labels.
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "no_progress" => Self::NoProgress,
            "reviewer_reject" => Self::ReviewerReject,
            "looping" => Self::Looping,
            "soft_deadline" => Self::SoftDeadline,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for RejectedVerdictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Persisted record for the latest rejected submission fingerprint at the
/// **task** level.
///
/// This record is keyed by `task_id` so a fresh task run can reload the latest
/// rejected fingerprint
/// across redispatch boundaries. The live submit-work guard uses
/// [`Self::diff_fingerprint`] to decide first-bounce (intercept + corrective
/// tool-result) vs second-strike (typed `no_progress_submission` settle)
/// behavior, and [`Self::no_progress_streak`] to drive streak increment/reset
/// semantics.
///
/// `review_id` is kept as a plain `Option<String>` so deleting a related
/// review row cannot orphan this durable integrity state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct TaskRejectedSubmissionIntegrityRecord {
    pub id: String,
    pub task_id: String,
    pub task_run_id: Option<String>,
    /// Associated review identifier, if this rejection was produced by a
    /// review path.
    pub review_id: Option<String>,
    /// Wire label of the rejection verdict (see [`RejectedVerdictKind`]).
    pub verdict_kind: String,
    /// Optional activity row that captured the rejection event.
    pub activity_id: Option<String>,
    /// ISO-8601 UTC timestamp of the rejection itself.
    pub rejected_at: String,
    /// Rejected submission's diff fingerprint (shared helper digest or legacy
    /// short fingerprint).
    pub diff_fingerprint: String,
    /// Task-level consecutive no-progress count as of this rejection.
    pub no_progress_streak: i32,
    /// Row creation timestamp; used for latest-wins tie-breaks.
    pub created_at: String,
}
