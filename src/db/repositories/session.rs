use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::session::{SessionRecord, SessionStatus};

pub struct SessionRepository {
    db: Database,
    events: broadcast::Sender<DjinnEvent>,
}

impl SessionRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self { db, events }
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
        continuation_of: Option<&str>,
    ) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();

        sqlx::query(
            "INSERT INTO sessions
                (id, project_id, task_id, model_id, agent_type, status, worktree_path, goose_session_id, continuation_of)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(task_id)
        .bind(model_id)
        .bind(agent_type)
        .bind(worktree_path)
        .bind(goose_session_id)
        .bind(continuation_of)
        .execute(self.db.pool())
        .await?;

        let session = sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE id = ?1",
        )
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

        let session = sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self
            .events
            .send(DjinnEvent::SessionUpdated(session.clone()));
        Ok(session)
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
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE id = ?1",
        )
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
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE project_id = ?1 AND id = ?2",
        )
        .bind(project_id)
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE task_id = ?1
             ORDER BY started_at DESC",
        )
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
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE project_id = ?1 AND task_id = ?2
             ORDER BY started_at DESC",
        )
        .bind(project_id)
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_active(&self) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE status = 'running'
             ORDER BY started_at DESC",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_active_in_project(&self, project_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE project_id = ?1 AND status = 'running'
             ORDER BY started_at DESC",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn active_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE task_id = ?1 AND status = 'running'
             ORDER BY started_at DESC
             LIMIT 1",
        )
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

    /// Return sessions for a task in continuation-chain order (root first, each subsequent
    /// session linked via `continuation_of`).  Handles chains of any length (A → B → C …).
    pub async fn chain_for_task(&self, task_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(
            "WITH RECURSIVE chain AS (
               SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                      status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
               FROM sessions
               WHERE task_id = ?1 AND continuation_of IS NULL
               UNION ALL
               SELECT s.id, s.project_id, s.task_id, s.model_id, s.agent_type, s.started_at, s.ended_at,
                      s.status, s.tokens_in, s.tokens_out, s.worktree_path, s.goose_session_id, s.continuation_of
               FROM sessions s JOIN chain c ON s.continuation_of = c.id
             )
             SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM chain
             ORDER BY started_at",
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Number of compacted (continuation) sessions for a task.
    pub async fn compaction_count(&self, task_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sessions WHERE task_id = ?1 AND continuation_of IS NOT NULL",
        )
        .bind(task_id)
        .fetch_one(self.db.pool())
        .await?)
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

        let session = sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self
            .events
            .send(DjinnEvent::SessionUpdated(session.clone()));
        Ok(session)
    }

    /// Set a paused session back to Running (for resume cycles).
    pub async fn set_running(&self, id: &str) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query("UPDATE sessions SET status = 'running' WHERE id = ?1")
            .bind(id)
            .execute(self.db.pool())
            .await?;

        let session = sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(self.db.pool())
        .await?;

        let _ = self
            .events
            .send(DjinnEvent::SessionUpdated(session.clone()));
        Ok(session)
    }

    /// Find the most recent paused session for a task (if any).
    pub async fn paused_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(
            "SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path, goose_session_id, continuation_of
             FROM sessions
             WHERE task_id = ?1 AND status = 'paused'
             ORDER BY started_at DESC
             LIMIT 1",
        )
        .bind(task_id)
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

    #[tokio::test]
    async fn create_and_update_emit_events() {
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
                None,
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

    #[tokio::test]
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

    #[tokio::test]
    async fn chain_query_and_compaction_count() {
        let db = test_helpers::create_test_db();
        let (tx, _) = broadcast::channel(1024);
        let (project_id, task_id) = create_task(tx.clone(), db.clone()).await;
        let repo = SessionRepository::new(db, tx);

        // A: root session
        let a = repo
            .create(
                &project_id,
                &task_id,
                "openai/gpt-5",
                "worker",
                None,
                None,
                None,
            )
            .await
            .unwrap();

        // B: compacted from A
        let b = repo
            .create(
                &project_id,
                &task_id,
                "openai/gpt-5",
                "worker",
                None,
                None,
                Some(&a.id),
            )
            .await
            .unwrap();

        // C: compacted from B
        let c = repo
            .create(
                &project_id,
                &task_id,
                "openai/gpt-5",
                "worker",
                None,
                None,
                Some(&b.id),
            )
            .await
            .unwrap();

        let chain = repo.chain_for_task(&task_id).await.unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, a.id);
        assert_eq!(chain[1].id, b.id);
        assert_eq!(chain[2].id, c.id);

        let count = repo.compaction_count(&task_id).await.unwrap();
        assert_eq!(count, 2); // B and C have continuation_of set
    }
}
