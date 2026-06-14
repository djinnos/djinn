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
    /// Running total of prompt-cache reads (cache hits) and writes (cache
    /// creation) across the session, persisted so cache hit-rate is queryable
    /// from the DB even when OTel/Langfuse telemetry is not configured.
    /// Added in migration 52.
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
    /// FK into `task_runs`; populated by the supervisor. The authoritative
    /// workspace path lives on the task_run row. Before migration 6 this
    /// struct also carried a `worktree_path: Option<String>` field mirroring
    /// the now-dropped `sessions.worktree_path` column.
    pub task_run_id: Option<String>,
    /// Human-readable title.  Populated (and auto-generated) for
    /// `agent_type='chat'` sessions; `NULL` for every other agent type.
    /// Added in migration 16.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional reason a terminal session was deliberately parked instead of
    /// being treated as an ordinary completion/failure. Added in migration 58.
    #[serde(default)]
    pub parked_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::SessionRecord;

    fn session_record(parked_reason: Option<String>) -> SessionRecord {
        SessionRecord {
            id: "session-1".to_owned(),
            project_id: Some("project-1".to_owned()),
            task_id: Some("task-1".to_owned()),
            model_id: "model".to_owned(),
            agent_type: "worker".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            ended_at: None,
            status: "completed".to_owned(),
            tokens_in: 1,
            tokens_out: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            task_run_id: None,
            title: None,
            parked_reason,
        }
    }

    #[test]
    fn session_record_serde_round_trips_without_parked_reason() {
        let record = session_record(None);

        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: SessionRecord = serde_json::from_str(&encoded).unwrap();

        assert!(decoded.parked_reason.is_none());
        assert_eq!(decoded.id, record.id);
    }

    #[test]
    fn session_record_serde_round_trips_with_parked_reason() {
        let record = session_record(Some("budget".to_owned()));

        let encoded = serde_json::to_string(&record).unwrap();
        let decoded: SessionRecord = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.parked_reason.as_deref(), Some("budget"));
        assert_eq!(decoded.id, record.id);
    }

    #[test]
    fn session_record_defaults_missing_parked_reason() {
        let json = serde_json::json!({
            "id": "session-1",
            "project_id": "project-1",
            "task_id": "task-1",
            "model_id": "model",
            "agent_type": "worker",
            "started_at": "2026-01-02T03:04:05.000Z",
            "ended_at": null,
            "status": "completed",
            "tokens_in": 1,
            "tokens_out": 2,
            "cache_read_tokens": 3,
            "cache_write_tokens": 4,
            "task_run_id": null,
            "title": null
        });

        let decoded: SessionRecord = serde_json::from_value(json).unwrap();

        assert!(decoded.parked_reason.is_none());
    }
}
