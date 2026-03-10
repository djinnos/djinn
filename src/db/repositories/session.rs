use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::session::{SessionRecord, SessionStatus};

/// Column list shared by all session SELECT queries.
const SESSION_COLS: &str =
    "id, project_id, task_id, model_id, agent_type, started_at, ended_at, \
     status, tokens_in, tokens_out, worktree_path, goose_session_id";

pub struct SessionRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl SessionRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
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
        let _ = self
            .events
            .send(DjinnEvent::SessionUpdated(session.clone()));
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        project_id: &str,
        task_id: &str,
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

        let _ = self
            .events
            .send(DjinnEvent::SessionCreated(session.clone()));
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
        let result = sqlx::query(
            "UPDATE sessions
             SET status = 'interrupted',
                 ended_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE status = 'running'",
        )
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn get(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, SessionRecord>(&format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"
            ))
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?,
        )
    }

    pub async fn get_in_project(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, SessionRecord>(&format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE project_id = ?1 AND id = ?2"
            ))
            .bind(project_id)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?,
        )
    }

    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, SessionRecord>(&format!(
                "SELECT {SESSION_COLS} FROM sessions WHERE task_id = ?1 ORDER BY started_at DESC"
            ))
            .bind(task_id)
            .fetch_all(self.db.pool())
            .await?,
        )
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
    use super::*;
    use crate::db::repositories::epic::EpicRepository;
    use crate::db::repositories::task::TaskRepository;
    use crate::test_helpers;

    async fn create_task(
        repo_events: broadcast::Sender<DjinnEvent>,
        db: Database,
    ) -> (String, String) {
        let epic_repo = EpicRepository::new(db.clone(), repo_events.clone());
        let epic = epic_repo.create("Epic", "", "", "", "").await.unwrap();

        let task_repo = TaskRepository::new(db, repo_events);
        let task = task_repo
            .create(&epic.id, "Task", "", "", "task", 0, "")
            .await
            .unwrap();
        (task.project_id, task.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_emits_event() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let (project_id, task_id) = create_task(tx.clone(), db.clone()).await;
        let repo = SessionRepository::new(db, tx);

        let created = repo
            .create(
                &project_id,
                &task_id,
                "openai/gpt-5",
                "worker",
                Some("/tmp/djinn-worktree-task"),
                Some("goose-session-abc123"),
            )
            .await
            .unwrap();
        assert_eq!(created.status, "running");

        let mut created_seen = false;
        for _ in 0..8 {
            if let DjinnEvent::SessionCreated(s) = rx.recv().await.unwrap() {
                assert_eq!(s.id, created.id);
                created_seen = true;
                break;
            }
        }
        assert!(created_seen, "expected SessionCreated event");

        let updated = repo
            .update(&created.id, SessionStatus::Completed, 10, 20)
            .await
            .unwrap();
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.tokens_in, 10);
        assert_eq!(updated.tokens_out, 20);
        assert!(updated.ended_at.is_some());

        let mut updated_seen = false;
        for _ in 0..8 {
            if let DjinnEvent::SessionUpdated(s) = rx.recv().await.unwrap() {
                assert_eq!(s.id, created.id);
                updated_seen = true;
                break;
            }
        }
        assert!(updated_seen, "expected SessionUpdated event");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_and_active_queries() {
        let db = test_helpers::create_test_db();
        let (tx, _) = broadcast::channel(1024);
        let (project_id, task_id) = create_task(tx.clone(), db.clone()).await;
        let repo = SessionRepository::new(db, tx);

        let first = repo
            .create(
                &project_id,
                &task_id,
                "openai/gpt-5",
                "worker",
                None,
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = repo
            .create(
                &project_id,
                &task_id,
                "openai/gpt-5",
                "worker",
                None,
                None,
            )
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
