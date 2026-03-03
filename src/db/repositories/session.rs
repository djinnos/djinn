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

    pub async fn create(
        &self,
        task_id: &str,
        model_id: &str,
        agent_type: &str,
        worktree_path: Option<&str>,
    ) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();

        sqlx::query(
            "INSERT INTO sessions
                (id, task_id, model_id, agent_type, status, worktree_path)
             VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
        )
        .bind(&id)
        .bind(task_id)
        .bind(model_id)
        .bind(agent_type)
        .bind(worktree_path)
        .execute(self.db.pool())
        .await?;

        let session = sqlx::query_as::<_, SessionRecord>(
            "SELECT id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path
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
            "SELECT id, task_id, model_id, agent_type, started_at, ended_at,
                    status, tokens_in, tokens_out, worktree_path
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::epic::EpicRepository;
    use crate::db::repositories::task::TaskRepository;
    use crate::test_helpers;

    async fn create_task(repo_events: broadcast::Sender<DjinnEvent>, db: Database) -> String {
        let epic_repo = EpicRepository::new(db.clone(), repo_events.clone());
        let epic = epic_repo.create("Epic", "", "", "", "").await.unwrap();

        let task_repo = TaskRepository::new(db, repo_events);
        let task = task_repo
            .create(&epic.id, "Task", "", "", "task", 0, "")
            .await
            .unwrap();
        task.id
    }

    #[tokio::test]
    async fn create_and_update_emit_events() {
        let db = test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(1024);
        let task_id = create_task(tx.clone(), db.clone()).await;
        let repo = SessionRepository::new(db, tx);

        let created = repo
            .create(
                &task_id,
                "openai/gpt-5",
                "worker",
                Some("/tmp/djinn-worktree-task"),
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
}
