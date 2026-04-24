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
    /// `NULL` for `agent_type = 'chat'` (global user-scoped sessions); required
    /// for every other agent type. Enforced at the schema level via the
    /// `sessions_project_scope_by_agent_type` CHECK constraint (migration 14).
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// FK into `task_runs`; populated by the supervisor. The authoritative
    /// workspace path lives on the task_run row. Before migration 6 this
    /// struct also carried a `worktree_path: Option<String>` field mirroring
    /// the now-dropped `sessions.worktree_path` column.
    pub task_run_id: Option<String>,
}
