use sqlx::Row;
use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::error::Result;
use crate::events::DjinnEvent;
use crate::models::epic_review_batch::{EpicReviewBatch, EpicReviewBatchTask};

#[derive(Clone, Debug)]
pub struct QueuedBatchAnchor {
    pub batch_id: String,
    pub project_id: String,
    pub epic_id: String,
    pub task_id: String,
}

#[derive(Clone, Debug)]
pub struct InReviewBatchSession {
    pub batch_id: String,
    pub project_id: String,
    pub session_id: String,
}

#[derive(Clone, Debug)]
pub struct InReviewBatchAnchor {
    pub batch_id: String,
    pub project_id: String,
    pub task_id: String,
}

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

    pub async fn active_batch_for_task(&self, task_id: &str) -> Result<Option<EpicReviewBatch>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, EpicReviewBatch>(
            "SELECT b.id, b.project_id, b.epic_id, b.status, b.verdict_reason, b.session_id,
                    b.created_at, b.started_at, b.completed_at
             FROM epic_review_batches b
             JOIN epic_review_batch_tasks bt ON bt.batch_id = b.id
             WHERE bt.task_id = ?1
               AND b.status IN ('queued', 'in_review')
             ORDER BY b.created_at DESC
             LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn list_queued_anchors(
        &self,
        project_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<QueuedBatchAnchor>> {
        self.db.ensure_initialized().await?;
        let rows = if let Some(project_id) = project_id {
            sqlx::query(
                "SELECT b.id AS batch_id,
                        b.project_id AS project_id,
                        b.epic_id AS epic_id,
                        (
                            SELECT bt.task_id
                            FROM epic_review_batch_tasks bt
                            WHERE bt.batch_id = b.id
                            ORDER BY bt.created_at, bt.task_id
                            LIMIT 1
                        ) AS task_id
                 FROM epic_review_batches b
                 WHERE b.status = 'queued'
                   AND b.project_id = ?1
                 ORDER BY b.created_at
                 LIMIT ?2",
            )
            .bind(project_id)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?
        } else {
            sqlx::query(
                "SELECT b.id AS batch_id,
                        b.project_id AS project_id,
                        b.epic_id AS epic_id,
                        (
                            SELECT bt.task_id
                            FROM epic_review_batch_tasks bt
                            WHERE bt.batch_id = b.id
                            ORDER BY bt.created_at, bt.task_id
                            LIMIT 1
                        ) AS task_id
                 FROM epic_review_batches b
                 WHERE b.status = 'queued'
                 ORDER BY b.created_at
                 LIMIT ?1",
            )
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?
        };

        let mut anchors = Vec::new();
        for row in rows {
            let task_id: Option<String> = sqlx::Row::try_get(&row, "task_id")?;
            let Some(task_id) = task_id else {
                continue;
            };
            anchors.push(QueuedBatchAnchor {
                batch_id: sqlx::Row::try_get(&row, "batch_id")?,
                project_id: sqlx::Row::try_get(&row, "project_id")?,
                epic_id: sqlx::Row::try_get(&row, "epic_id")?,
                task_id,
            });
        }
        Ok(anchors)
    }

    pub async fn list_in_review_sessions(
        &self,
        project_id: Option<&str>,
    ) -> Result<Vec<InReviewBatchSession>> {
        self.db.ensure_initialized().await?;
        let rows = if let Some(project_id) = project_id {
            sqlx::query(
                "SELECT id AS batch_id, project_id, session_id
                 FROM epic_review_batches
                 WHERE status = 'in_review'
                   AND project_id = ?1
                   AND session_id IS NOT NULL
                 ORDER BY started_at, created_at",
            )
            .bind(project_id)
            .fetch_all(self.db.pool())
            .await?
        } else {
            sqlx::query(
                "SELECT id AS batch_id, project_id, session_id
                 FROM epic_review_batches
                 WHERE status = 'in_review'
                   AND session_id IS NOT NULL
                 ORDER BY started_at, created_at",
            )
            .fetch_all(self.db.pool())
            .await?
        };

        let mut out = Vec::new();
        for row in rows {
            let session_id: Option<String> = row.try_get("session_id")?;
            let Some(session_id) = session_id else {
                continue;
            };
            out.push(InReviewBatchSession {
                batch_id: row.try_get("batch_id")?,
                project_id: row.try_get("project_id")?,
                session_id,
            });
        }
        Ok(out)
    }

    pub async fn list_in_review_anchors(
        &self,
        project_id: Option<&str>,
    ) -> Result<Vec<InReviewBatchAnchor>> {
        self.db.ensure_initialized().await?;
        let rows = if let Some(project_id) = project_id {
            sqlx::query(
                "SELECT b.id AS batch_id,
                        b.project_id AS project_id,
                        (
                            SELECT bt.task_id
                            FROM epic_review_batch_tasks bt
                            WHERE bt.batch_id = b.id
                            ORDER BY bt.created_at, bt.task_id
                            LIMIT 1
                        ) AS task_id
                 FROM epic_review_batches b
                 WHERE b.status = 'in_review'
                   AND b.project_id = ?1
                 ORDER BY b.started_at, b.created_at",
            )
            .bind(project_id)
            .fetch_all(self.db.pool())
            .await?
        } else {
            sqlx::query(
                "SELECT b.id AS batch_id,
                        b.project_id AS project_id,
                        (
                            SELECT bt.task_id
                            FROM epic_review_batch_tasks bt
                            WHERE bt.batch_id = b.id
                            ORDER BY bt.created_at, bt.task_id
                            LIMIT 1
                        ) AS task_id
                 FROM epic_review_batches b
                 WHERE b.status = 'in_review'
                 ORDER BY b.started_at, b.created_at",
            )
            .fetch_all(self.db.pool())
            .await?
        };

        let mut out = Vec::new();
        for row in rows {
            let task_id: Option<String> = row.try_get("task_id")?;
            let Some(task_id) = task_id else {
                continue;
            };
            out.push(InReviewBatchAnchor {
                batch_id: row.try_get("batch_id")?,
                project_id: row.try_get("project_id")?,
                task_id,
            });
        }
        Ok(out)
    }

    pub async fn requeue(&self, batch_id: &str, reason: Option<&str>) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE epic_review_batches
             SET status = 'queued',
                 verdict_reason = COALESCE(?2, verdict_reason),
                 session_id = NULL,
                 started_at = NULL
             WHERE id = ?1",
        )
        .bind(batch_id)
        .bind(reason)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}
