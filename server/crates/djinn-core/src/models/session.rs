use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
    Paused,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Paused => "paused",
        }
    }
}

/// Persisted lifecycle record for a supervisor-run agent session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct SessionRecord {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Legacy per-session worktree path. Migration 6 will drop this column;
    /// consumers needing the on-disk workspace must read
    /// `task_runs.workspace_path` via `task_run_id` instead.
    pub worktree_path: Option<String>,
    /// FK into `task_runs`; NULL until the supervisor populates it. When
    /// present, the authoritative workspace path lives on the task_run row.
    pub task_run_id: Option<String>,
}
