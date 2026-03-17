use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::{SessionRecord, SessionStatus};

use crate::database::Database;
use crate::Result;

/// Column list shared by all session SELECT queries.
const SESSION_COLS: &str = "id, project_id, task_id, model_id, agent_type, started_at, ended_at, \
     status, tokens_in, tokens_out, worktree_path, goose_session_id";

pub struct SessionRepository {
    db: Database,
    events: EventBus,
}

impl SessionRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Re-fetch a session by id and emit `SessionUpdated`.
    async fn fetch_and_emit_update(&self, id: &str) -> Result<SessionRecord> {
        let session = sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"
        ))
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;
        let action = match session.status.as_str() {
            "running" => "started",
            "completed" => "completed",
            "interrupted" => "interrupted",
            "failed" => "failed",
            _ => "updated",
        };
        self.events.send(DjinnEventEnvelope {
            entity_type: "session",
            action,
            payload: serde_json::to_value(&session).unwrap_or_default(),
            id: None,
            project_id: None,
            from_sync: false,
        });
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        project_id: &str,
        task_id: Option<&str>,
        model_id: &str,
        agent_type: &str,
        worktree_path: Option<&str>,
        goose_session_id: Option<&str>,
    ) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();

        sqlx::query(
            "INSERT INTO sessions
                (id, project_id, task_id, model_id, agent_type, status, worktree_path, goose_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(task_id)
        .bind(model_id)
        .bind(agent_type)
        .bind(worktree_path)
        .bind(goose_session_id)
        .execute(self.db.pool())
        .await?;

        let session = sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"
        ))
        .bind(&id)
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope {
            entity_type: "session",
            action: "started",
            payload: serde_json::to_value(&session).unwrap_or_default(),
            id: None,
            project_id: None,
            from_sync: false,
        });
        tracing::info!(
            session_id = %session.id,
            task_id = ?session.task_id,
            "SessionRepository: emitted session.started SSE event"
        );
        Ok(session)
    }

    pub async fn update(
        &self,
        id: &str,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
    ) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query(
            "UPDATE sessions
             SET status = ?2,
                 tokens_in = ?3,
                 tokens_out = ?4,
                 ended_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(tokens_in)
        .bind(tokens_out)
        .execute(self.db.pool())
        .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Mark all `running` sessions as `interrupted`.
    /// Called once at server startup — no runtime sessions can exist yet.
    pub async fn interrupt_all_running(&self) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let running_sessions = sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE status = 'running'"
        ))
        .fetch_all(self.db.pool())
        .await?;

        if running_sessions.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query(
            "UPDATE sessions
             SET status = 'interrupted',
                 ended_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE status = 'running'",
        )
        .execute(self.db.pool())
        .await?;

        for session in running_sessions {
            let _ = self.fetch_and_emit_update(&session.id).await?;
        }

        Ok(result.rows_affected())
    }

    pub async fn get(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_in_project(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE project_id = ?1 AND id = ?2"
        ))
        .bind(project_id)
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE task_id = ?1 ORDER BY started_at DESC"
        ))
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_for_task_in_project(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions \
             WHERE project_id = ?1 AND task_id = ?2 ORDER BY started_at DESC"
        ))
        .bind(project_id)
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_active(&self) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions \
             WHERE status = 'running' ORDER BY started_at DESC"
        ))
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_active_in_project(&self, project_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions \
             WHERE project_id = ?1 AND status = 'running' ORDER BY started_at DESC"
        ))
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn active_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions \
             WHERE task_id = ?1 AND status = 'running' ORDER BY started_at DESC LIMIT 1"
        ))
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn count_for_task(&self, task_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions WHERE task_id = ?1")
                .bind(task_id)
                .fetch_one(self.db.pool())
                .await?,
        )
    }

    /// Batch count sessions per task for a list of task IDs.
    pub async fn count_for_tasks(
        &self,
        task_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, i64>> {
        self.db.ensure_initialized().await?;
        if task_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders: Vec<String> = (1..=task_ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT task_id, COUNT(*) as cnt FROM sessions WHERE task_id IN ({}) GROUP BY task_id",
            placeholders.join(", ")
        );
        let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
        for id in task_ids {
            q = q.bind(*id);
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        Ok(rows.into_iter().collect())
    }

    /// Set session status to Paused without setting ended_at.
    /// Used when a worker completes (Done) but its worktree is kept alive for the review cycle.
    pub async fn pause(&self, id: &str, tokens_in: i64, tokens_out: i64) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query(
            "UPDATE sessions SET status = 'paused', tokens_in = ?2, tokens_out = ?3 WHERE id = ?1",
        )
        .bind(id)
        .bind(tokens_in)
        .bind(tokens_out)
        .execute(self.db.pool())
        .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Set a paused session back to Running (for resume cycles).
    pub async fn set_running(&self, id: &str) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query("UPDATE sessions SET status = 'running' WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Find the most recent paused session for a task (if any).
    pub async fn paused_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions \
             WHERE task_id = ?1 AND status = 'paused' ORDER BY started_at DESC LIMIT 1"
        ))
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Find the most recent paused session for a task that matches the given
    /// agent type.  Used during dispatch so that e.g. a PM session never
    /// accidentally resumes a worker's paused conversation.
    pub async fn paused_for_task_by_type(
        &self,
        task_id: &str,
        agent_type: &str,
    ) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(&format!(
            "SELECT {SESSION_COLS} FROM sessions \
             WHERE task_id = ?1 AND status = 'paused' AND agent_type = ?2 \
             ORDER BY started_at DESC LIMIT 1"
        ))
        .bind(task_id)
        .bind(agent_type)
        .fetch_optional(self.db.pool())
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::SessionRecord;

    use super::*;
    use crate::repositories::epic::EpicRepository;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn capturing_bus() -> (EventBus, Arc<Mutex<Vec<DjinnEventEnvelope>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        (bus, captured)
    }

    /// Create a task via raw SQL (no TaskRepository dep), returns (project_id, task_id).
    async fn create_task(db: &Database, bus: EventBus) -> (String, String) {
        let epic_repo = EpicRepository::new(db.clone(), bus);
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, memory_refs)
             VALUES (?1, ?2, 'tsst', ?3, 'Task', '', '', 'task', 0, '', 'backlog', 0, '[]')",
        )
        .bind(&task_id)
        .bind(&epic.project_id)
        .bind(&epic.id)
        .execute(db.pool())
        .await
        .unwrap();

        (epic.project_id, task_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_emits_event() {
        let db = test_db();
        let (bus, captured) = capturing_bus();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db, bus);

        let created = repo
            .create(
                &project_id,
                Some(&task_id),
                "openai/gpt-5",
                "worker",
                Some("/tmp/djinn-worktree-task"),
                Some("goose-session-abc123"),
            )
            .await
            .unwrap();
        assert_eq!(created.status, "running");

        let events = captured.lock().unwrap();
        let started = events.iter().find(|e| e.entity_type == "session" && e.action == "started");
        assert!(started.is_some(), "expected session.started event");
        let s: SessionRecord = serde_json::from_value(started.unwrap().payload.clone()).unwrap();
        assert_eq!(s.id, created.id);
        drop(events);

        captured.lock().unwrap().clear();

        let updated = repo
            .update(&created.id, SessionStatus::Completed, 10, 20)
            .await
            .unwrap();
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.tokens_in, 10);
        assert_eq!(updated.tokens_out, 20);
        assert!(updated.ended_at.is_some());

        let events = captured.lock().unwrap();
        let completed = events.iter().find(|e| e.entity_type == "session" && e.action == "completed");
        assert!(completed.is_some(), "expected session.completed event");
        let s: SessionRecord = serde_json::from_value(completed.unwrap().payload.clone()).unwrap();
        assert_eq!(s.id, created.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_all_running_emits_session_updated_for_each_interrupted_session() {
        let db = test_db();
        let (bus, captured) = capturing_bus();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db, bus);

        let first = repo
            .create(&project_id, Some(&task_id), "openai/gpt-5", "worker", None, None)
            .await
            .unwrap();
        let second = repo
            .create(&project_id, Some(&task_id), "openai/gpt-5", "worker", None, None)
            .await
            .unwrap();

        captured.lock().unwrap().clear();

        let rows = repo.interrupt_all_running().await.unwrap();
        assert_eq!(rows, 2);

        let events = captured.lock().unwrap();
        let interrupted: Vec<_> = events.iter()
            .filter(|e| e.entity_type == "session" && e.action == "interrupted")
            .collect();
        assert_eq!(interrupted.len(), 2);

        let ids: std::collections::HashSet<String> = interrupted.iter()
            .map(|e| {
                let s: SessionRecord = serde_json::from_value(e.payload.clone()).unwrap();
                assert_eq!(s.status, "interrupted");
                assert!(s.ended_at.is_some());
                s.id
            })
            .collect();
        assert!(ids.contains(&first.id));
        assert!(ids.contains(&second.id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_and_active_queries() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db, EventBus::noop());

        let first = repo
            .create(&project_id, Some(&task_id), "openai/gpt-5", "worker", None, None)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = repo
            .create(&project_id, Some(&task_id), "openai/gpt-5", "worker", None, None)
            .await
            .unwrap();

        let listed = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id);
        assert_eq!(listed[1].id, first.id);

        let count = repo.count_for_task(&task_id).await.unwrap();
        assert_eq!(count, 2);

        let active = repo.list_active().await.unwrap();
        assert_eq!(active.len(), 2);

        let active_task = repo.active_for_task(&task_id).await.unwrap();
        assert_eq!(active_task.unwrap().id, second.id);

        let _ = repo
            .update(&second.id, SessionStatus::Completed, 1, 1)
            .await
            .unwrap();
        let active_task = repo.active_for_task(&task_id).await.unwrap();
        assert_eq!(active_task.unwrap().id, first.id);
    }
}
