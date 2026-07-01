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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
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
