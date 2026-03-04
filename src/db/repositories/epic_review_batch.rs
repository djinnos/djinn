use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::epic_review_batch::{EpicReviewBatch, EpicReviewBatchTask};

pub struct EpicReviewBatchRepository {
    db: Database,
    _events: broadcast::Sender<DjinnEvent>,
}

impl EpicReviewBatchRepository {
    pub fn new(db: Database, events: broadcast::Sender<DjinnEvent>) -> Self {
        Self {
            db,
            _events: events,
        }
    }

    pub async fn create_batch(
        &self,
        project_id: &str,
        epic_id: &str,
        task_ids: &[String],
    ) -> Result<EpicReviewBatch> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO epic_review_batches (id, project_id, epic_id, status)
             VALUES (?1, ?2, ?3, 'queued')",
        )
        .bind(&id)
        .bind(project_id)
        .bind(epic_id)
        .execute(&mut *tx)
        .await?;

        for task_id in task_ids {
            sqlx::query(
                "INSERT INTO epic_review_batch_tasks (batch_id, task_id)
                 VALUES (?1, ?2)",
            )
            .bind(&id)
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
        }

        let batch: EpicReviewBatch = sqlx::query_as(
            "SELECT id, project_id, epic_id, status, verdict_reason, session_id,
                    created_at, started_at, completed_at
             FROM epic_review_batches WHERE id = ?1",
        )
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(batch)
    }

    pub async fn list_batch_tasks(&self, batch_id: &str) -> Result<Vec<EpicReviewBatchTask>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, EpicReviewBatchTask>(
            "SELECT batch_id, task_id, created_at
             FROM epic_review_batch_tasks
             WHERE batch_id = ?1
             ORDER BY created_at, task_id",
        )
        .bind(batch_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn has_active_batch(&self, epic_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM epic_review_batches
             WHERE epic_id = ?1
               AND status IN ('queued', 'in_review')",
        )
        .bind(epic_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(count > 0)
    }

    pub async fn list_unreviewed_closed_task_ids(&self, epic_id: &str) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT t.id
             FROM tasks t
             WHERE t.epic_id = ?1
               AND t.status = 'closed'
               AND NOT EXISTS (
                 SELECT 1
                 FROM epic_review_batch_tasks bt
                 JOIN epic_review_batches b ON b.id = bt.batch_id
                 WHERE bt.task_id = t.id
                   AND b.status = 'clean'
               )
             ORDER BY t.updated_at, t.id",
        )
        .bind(epic_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn mark_in_review(&self, batch_id: &str, session_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epic_review_batches
             SET status = 'in_review',
                 session_id = ?2,
                 started_at = COALESCE(started_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             WHERE id = ?1",
        )
        .bind(batch_id)
        .bind(session_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_clean(&self, batch_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epic_review_batches
             SET status = 'clean',
                 completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(batch_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_issues_found(&self, batch_id: &str, reason: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epic_review_batches
             SET status = 'issues_found',
                 verdict_reason = ?2,
                 completed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(batch_id)
        .bind(reason)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}
