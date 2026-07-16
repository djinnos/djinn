use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Source that produced the canonical verify result.
///
/// Wire strings are the lowercase variant names stored as VARCHAR(64).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifySource {
    /// Verification ran by CI (GitHub Actions, etc.).
    Ci,
    /// Verification ran locally inside the worker pod/container.
    Local,
    /// Verification ran by the worker agent itself.
    Worker,
}

impl VerifySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ci => "ci",
            Self::Local => "local",
            Self::Worker => "worker",
        }
    }
}

impl fmt::Display for VerifySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VerifySource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ci" => Ok(Self::Ci),
            "local" => Ok(Self::Local),
            "worker" => Ok(Self::Worker),
            other => Err(format!("unknown verify source: {other}")),
        }
    }
}

/// Outcome of a verify run.
///
/// Wire strings are the lowercase variant names stored as VARCHAR(32).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyResult {
    /// All checks passed.
    Pass,
    /// One or more checks failed.
    Fail,
    /// Verify run errored out (infra failure, timeout, etc.).
    Error,
}

impl VerifyResult {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for VerifyResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for VerifyResult {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pass" => Ok(Self::Pass),
            "fail" => Ok(Self::Fail),
            "error" => Ok(Self::Error),
            other => Err(format!("unknown verify result: {other}")),
        }
    }
}

/// Trigger reason for an auto-submit decision.
///
/// Wire strings are the lowercase variant names stored as VARCHAR(64).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutoSubmitTriggerReason {
    /// Session became idle (no new tool calls / messages for the configured window).
    Idle,
    /// Model is looping — repeated identical turns detected.
    Looping,
    /// No-progress streak exceeded the configured threshold.
    NoProgress,
    /// Soft deadline reached; session is entering controlled termination.
    SoftDeadline,
    /// Hard termination signal received; final attempt before shutdown.
    ControlledTermination,
}

impl AutoSubmitTriggerReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Looping => "looping",
            Self::NoProgress => "no_progress",
            Self::SoftDeadline => "soft_deadline",
            Self::ControlledTermination => "controlled_termination",
        }
    }
}

impl fmt::Display for AutoSubmitTriggerReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AutoSubmitTriggerReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "idle" => Ok(Self::Idle),
            "looping" => Ok(Self::Looping),
            "no_progress" => Ok(Self::NoProgress),
            "soft_deadline" => Ok(Self::SoftDeadline),
            "controlled_termination" => Ok(Self::ControlledTermination),
            other => Err(format!("unknown auto-submit trigger reason: {other}")),
        }
    }
}

/// Persisted record for a canonical verify run attached to a task_run.
///
/// Captures the identity, versioning, timing, result, diff fingerprint, and
/// task-specific check coverage of the verification that produced the
/// authoritative pass/fail signal used by auto-submit decisions.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[cfg_attr(feature = "sqlx", sqlx(default))]
pub struct VerifyRunRecord {
    pub id: String,
    pub task_run_id: String,
    pub verify_source: String,
    pub verify_run_id: String,
    pub command_version: Option<String>,
    pub profile_version: Option<String>,
    pub completed_at: String,
    pub result: String,
    pub diff_fingerprint: String,
    /// JSON object encoding per-check coverage (e.g. `{"lint": true, "test": true}`).
    pub check_coverage: Option<serde_json::Value>,
    /// Phase that produced this record. Legacy records have no phase.
    pub source_phase: Option<String>,
    /// Durable identifier for the final-verification attempt.
    pub verification_attempt_id: Option<String>,
    /// Ordered command descriptors and their results for an atomic attempt.
    pub ordered_commands: Option<serde_json::Value>,
    /// Ordered check IDs covered by the completed attempt.
    pub covered_checks: Option<serde_json::Value>,
    /// Complete fingerprint of all inputs to final verification.
    pub verification_input_fingerprint: Option<String>,
    /// Exact version of the input manifest used to derive the fingerprint.
    pub manifest_version: Option<String>,
    /// Canonical JSON representation of the execution environment identity.
    pub environment_identity_json: Option<serde_json::Value>,
    /// Digest of the canonical environment identity JSON.
    pub environment_identity_digest: Option<String>,
    /// Version of the environment identity canonicalization contract.
    pub environment_identity_version: Option<String>,
    pub created_at: String,
}

/// Persisted record for an auto-submit review.
///
/// Captures all metadata needed for later audit and freshness evaluation:
/// trigger reason, diff fingerprint, verify linkage, session/model identity,
/// no-progress streak counter, and whether the model invoked `submit_work`
/// itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct AutoSubmitReviewRecord {
    pub id: String,
    pub task_run_id: String,
    pub trigger_reason: String,
    pub diff_fingerprint: String,
    pub verify_source: Option<String>,
    pub verify_run_id: Option<String>,
    pub verify_timestamp: Option<String>,
    pub session_id: Option<String>,
    pub model_id: Option<String>,
    pub no_progress_streak: i32,
    pub model_called_submit_work: bool,
    pub created_at: String,
}

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
    /// No-progress streak crossed the configured threshold (auto-submit path).
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
/// Unlike [`AutoSubmitReviewRecord`] (task-run-scoped), this record is keyed
/// by `task_id` so a fresh task run can reload the latest rejected fingerprint
/// across redispatch boundaries. The live submit-work guard uses
/// [`Self::diff_fingerprint`] to decide first-bounce (intercept + corrective
/// tool-result) vs second-strike (typed `no_progress_submission` settle)
/// behavior, and [`Self::no_progress_streak`] to drive streak increment/reset
/// semantics.
///
/// `review_id` is kept as a plain `Option<String>` (not an FK to
/// `auto_submit_reviews.id`) so deleting a review row cannot orphan this
/// durable integrity state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct TaskRejectedSubmissionIntegrityRecord {
    pub id: String,
    pub task_id: String,
    pub task_run_id: Option<String>,
    /// Associated `auto_submit_reviews.id` (or equivalent review identifier),
    /// if this rejection was produced by a review path.
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
