use djinn_core::models::TaskRejectedSubmissionIntegrityRecord;

use crate::Result;
use crate::database::Database;

// ─── TaskRejectedSubmissionIntegrityRepository ───────────────────────────────

/// Repository for durable task-level rejected submission fingerprints.
///
/// The live submit-work guard reloads the latest rejected fingerprint by
/// `task_id` across redispatch / new task-run boundaries. This repository
/// owns the `task_rejected_submission_integrity` table added in migration 91.
pub struct TaskRejectedSubmissionIntegrityRepository {
    db: Database,
}

/// Parameters for recording a new rejected submission fingerprint at the
/// task level.
///
/// `task_id` is required and durable across task runs; `task_run_id`,
/// `review_id`, and `activity_id` are optional associations for callers that
/// only know the task identity. `no_progress_streak` is the task-level streak
/// value as of this rejection; the repository does not mutate it on insert.
pub struct RecordTaskRejectedSubmissionParams<'a> {
    pub id: &'a str,
    pub task_id: &'a str,
    pub task_run_id: Option<&'a str>,
    pub review_id: Option<&'a str>,
    pub verdict_kind: &'a str,
    pub activity_id: Option<&'a str>,
    pub rejected_at: &'a str,
    pub diff_fingerprint: &'a str,
    pub no_progress_streak: i32,
}

impl TaskRejectedSubmissionIntegrityRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Persist a new rejected-submission fingerprint row at the task level.
    ///
    /// Multiple rows per `task_id` are permitted; [`Self::latest_for_task`]
    /// picks the most recent by `rejected_at` (then `created_at` as a
    /// tie-break), so this method is append-only.
    pub async fn record(
        &self,
        params: RecordTaskRejectedSubmissionParams<'_>,
    ) -> Result<TaskRejectedSubmissionIntegrityRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "INSERT INTO task_rejected_submission_integrity
                (id, task_id, task_run_id, review_id, verdict_kind,
                 activity_id, rejected_at, diff_fingerprint, no_progress_streak)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            params.id,
            params.task_id,
            params.task_run_id,
            params.review_id,
            params.verdict_kind,
            params.activity_id,
            params.rejected_at,
            params.diff_fingerprint,
            params.no_progress_streak,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity WHERE id = $1"#,
            params.id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Return the latest rejected submission fingerprint for a task.
    ///
    /// Returns `None` when no rejected fingerprint has ever been recorded for
    /// `task_id` — the explicit no-comparison path used by the live submit-work
    /// guard so historical state is never fabricated.
    ///
    /// Ordering is `rejected_at DESC, created_at DESC`: `rejected_at` is the
    /// authoritative rejection timestamp (the activity/verdict event), while
    /// `created_at` is the tie-break for rows recorded in the same wall-clock
    /// instant.
    pub async fn latest_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<TaskRejectedSubmissionIntegrityRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity
               WHERE task_id = $1
               ORDER BY rejected_at DESC, created_at DESC
               LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return the latest task-level no-progress streak for a task.
    ///
    /// Mirrors [`Self::latest_for_task`] but returns only the streak value,
    /// defaulting to `0` when no rejected fingerprint exists (the
    /// no-comparison path).
    pub async fn latest_no_progress_streak_for_task(&self, task_id: &str) -> Result<i32> {
        Ok(self
            .latest_for_task(task_id)
            .await?
            .map(|r| r.no_progress_streak)
            .unwrap_or(0))
    }

    /// Return all rejected-submission fingerprint rows for a task, newest
    /// first.
    pub async fn list_for_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<TaskRejectedSubmissionIntegrityRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity
               WHERE task_id = $1
               ORDER BY rejected_at DESC, created_at DESC"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Reset the task-level no-progress streak to zero.
    ///
    /// Semantically this records a sentinel row with the *current* (incoming)
    /// diff fingerprint and a zero streak, so subsequent [`Self::latest_for_task`]
    /// lookups observe the reset. This keeps the append-only model honest: a
    /// reset is recorded, not silently mutated, so the audit trail is
    /// preserved.
    ///
    /// `reset_diff_fingerprint` is the fingerprint that triggered the reset
    /// (typically a fresh, progressed submission). Callers that do not have a
    /// fresh fingerprint should pass the *previous* latest fingerprint — the
    /// point of a reset is that streak semantics restart, not that the
    /// fingerprint changes.
    pub async fn reset_no_progress_streak(
        &self,
        task_id: &str,
        reset_diff_fingerprint: &str,
        reset_at: &str,
        task_run_id: Option<&str>,
    ) -> Result<TaskRejectedSubmissionIntegrityRecord> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO task_rejected_submission_integrity
                (id, task_id, task_run_id, verdict_kind,
                 rejected_at, diff_fingerprint, no_progress_streak)
             VALUES ($1, $2, $3, 'no_progress', $4, $5, 0)",
            id,
            task_id,
            task_run_id,
            reset_at,
            reset_diff_fingerprint,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Return a single record by its id.
    pub async fn get(&self, id: &str) -> Result<Option<TaskRejectedSubmissionIntegrityRecord>> {
        self.db.ensure_initialized().await?;

        Ok(sqlx::query_as!(
            TaskRejectedSubmissionIntegrityRecord,
            r#"SELECT id, task_id, task_run_id, review_id, verdict_kind,
                      activity_id, rejected_at, diff_fingerprint,
                      no_progress_streak, created_at
               FROM task_rejected_submission_integrity WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }
}

#[cfg(test)]
mod tests;
