use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Lifecycle status of a `task_run`.
///
/// Wire strings are the lowercase variant names; the DB stores them as
/// VARCHAR(64).  Terminal statuses (Completed, Failed, Interrupted) cause the
/// run's `ended_at` to be stamped; `Running` leaves it NULL.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl TaskRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    /// True for statuses that close out a run (i.e. should set `ended_at`).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

impl fmt::Display for TaskRunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskRunStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(format!("unknown task_run status: {other}")),
        }
    }
}

/// Why a task_run was created.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunTrigger {
    NewTask,
    ConflictRetry,
    ReviewResponse,
}

impl TaskRunTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NewTask => "new_task",
            Self::ConflictRetry => "conflict_retry",
            Self::ReviewResponse => "review_response",
        }
    }
}

impl fmt::Display for TaskRunTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskRunTrigger {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "new_task" => Ok(Self::NewTask),
            "conflict_retry" => Ok(Self::ConflictRetry),
            "review_response" => Ok(Self::ReviewResponse),
            other => Err(format!("unknown task_run trigger: {other}")),
        }
    }
}

/// Persisted record for one execution of a task — spanning planner → worker →
/// reviewer → verifier stages.  Child `sessions` rows reference the run via
/// `sessions.task_run_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct TaskRunRecord {
    pub id: String,
    pub project_id: String,
    pub task_id: String,
    pub trigger_type: String,
    pub status: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub workspace_path: Option<String>,
    pub mirror_ref: Option<String>,
}
