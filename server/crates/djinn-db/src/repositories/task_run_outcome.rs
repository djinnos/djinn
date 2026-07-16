//! Immutable task-run outcome facts; callers must provide exact identities.
use crate::{Result, database::Database, error::DbError};
use djinn_core::models::TaskRunOutcomeFact;

pub struct TaskRunOutcomeRepository {
    db: Database,
}
impl TaskRunOutcomeRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
    pub async fn get(&self, run_id: &str) -> Result<Option<TaskRunOutcomeFact>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as("SELECT task_run_id, attempt_seq, outcome, parked_reason, review_verdict, merge_queue_result, created_at, updated_at FROM task_run_outcome_facts WHERE task_run_id = $1").bind(run_id).fetch_optional(self.db.pool()).await?)
    }
    /// Associate this exact attempt and snapshot its ordinal. Contradictions fail.
    pub async fn create_for_attempt(
        &self,
        run_id: &str,
        attempt_id: &str,
    ) -> Result<TaskRunOutcomeFact> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let a: Option<(String, Option<String>, i32)> = sqlx::query_as(
            "SELECT task_id, task_run_id, attempt_seq FROM task_attempts WHERE id = $1 FOR UPDATE",
        )
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (task_id, associated, seq) =
            a.ok_or_else(|| DbError::InvalidData("task attempt does not exist".into()))?;
        let task: Option<String> =
            sqlx::query_scalar("SELECT task_id FROM task_runs WHERE id = $1 FOR UPDATE")
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await?;
        if task.as_deref() != Some(task_id.as_str())
            || associated.as_deref().is_some_and(|old| old != run_id)
        {
            return Err(DbError::InvalidData(
                "contradictory exact attempt/run association".into(),
            ));
        }
        sqlx::query("UPDATE task_attempts SET task_run_id = $1 WHERE id = $2")
            .bind(run_id)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO task_run_outcome_facts (task_run_id, attempt_seq, outcome) VALUES ($1, $2, 'observed') ON CONFLICT (task_run_id) DO NOTHING").bind(run_id).bind(seq).execute(&mut *tx).await?;
        let fact: TaskRunOutcomeFact = sqlx::query_as("SELECT task_run_id, attempt_seq, outcome, parked_reason, review_verdict, merge_queue_result, created_at, updated_at FROM task_run_outcome_facts WHERE task_run_id = $1").bind(run_id).fetch_one(&mut *tx).await?;
        if fact.attempt_seq != Some(seq) {
            return Err(DbError::InvalidData(
                "contradictory immutable attempt ordinal".into(),
            ));
        }
        tx.commit().await?;
        Ok(fact)
    }

    /// Write one observation exactly once. Same-value retries are accepted;
    /// a later contradictory observation is not allowed to rewrite history.
    async fn write_once(
        &self,
        run_id: &str,
        column: &str,
        value: &str,
    ) -> Result<TaskRunOutcomeFact> {
        self.db.ensure_initialized().await?;
        let sql = format!(
            "UPDATE task_run_outcome_facts SET {column} = $2, updated_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
             WHERE task_run_id = $1 AND ({column} IS NULL OR {column} = $2) \
             RETURNING task_run_id, attempt_seq, outcome, parked_reason, review_verdict, merge_queue_result, created_at, updated_at"
        );
        if let Some(fact) = sqlx::query_as(&sql)
            .bind(run_id)
            .bind(value)
            .fetch_optional(self.db.pool())
            .await?
        {
            return Ok(fact);
        }
        match self.get(run_id).await? {
            Some(_) => Err(DbError::InvalidData(format!(
                "contradictory immutable {column}"
            ))),
            None => Err(DbError::InvalidData(
                "task-run outcome fact does not exist".into(),
            )),
        }
    }

    pub async fn record_outcome(&self, run_id: &str, outcome: &str) -> Result<TaskRunOutcomeFact> {
        if !matches!(outcome, "legacy_unknown" | "observed") {
            return Err(DbError::InvalidData("invalid task-run outcome".into()));
        }
        self.write_once(run_id, "outcome", outcome).await
    }

    pub async fn record_parked_reason(
        &self,
        run_id: &str,
        reason: &str,
    ) -> Result<TaskRunOutcomeFact> {
        self.write_once(run_id, "parked_reason", reason).await
    }

    pub async fn record_review_verdict(
        &self,
        run_id: &str,
        verdict: &str,
    ) -> Result<TaskRunOutcomeFact> {
        if !matches!(verdict, "accepted" | "rejected" | "not_applicable") {
            return Err(DbError::InvalidData("invalid review verdict".into()));
        }
        self.write_once(run_id, "review_verdict", verdict).await
    }

    pub async fn record_merge_queue_result(
        &self,
        run_id: &str,
        result: &str,
    ) -> Result<TaskRunOutcomeFact> {
        if !matches!(result, "passed" | "failed" | "not_applicable") {
            return Err(DbError::InvalidData("invalid merge queue result".into()));
        }
        self.write_once(run_id, "merge_queue_result", result).await
    }
}
