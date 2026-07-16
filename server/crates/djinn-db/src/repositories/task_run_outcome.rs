//! Immutable task-run outcome facts; callers must provide exact identities.
use djinn_core::models::TaskRunOutcomeFact;
use crate::{database::Database, error::DbError, Result};

pub struct TaskRunOutcomeRepository { db: Database }
impl TaskRunOutcomeRepository {
    pub fn new(db: Database) -> Self { Self { db } }
    pub async fn get(&self, run_id: &str) -> Result<Option<TaskRunOutcomeFact>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as("SELECT task_run_id, attempt_seq, outcome, parked_reason, review_verdict, merge_queue_result, created_at, updated_at FROM task_run_outcome_facts WHERE task_run_id = $1").bind(run_id).fetch_optional(self.db.pool()).await?)
    }
    /// Associate this exact attempt and snapshot its ordinal. Contradictions fail.
    pub async fn create_for_attempt(&self, run_id: &str, attempt_id: &str) -> Result<TaskRunOutcomeFact> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let a: Option<(String, Option<String>, i32)> = sqlx::query_as("SELECT task_id, task_run_id, attempt_seq FROM task_attempts WHERE id = $1 FOR UPDATE").bind(attempt_id).fetch_optional(&mut *tx).await?;
        let (task_id, associated, seq) = a.ok_or_else(|| DbError::InvalidData("task attempt does not exist".into()))?;
        let task: Option<String> = sqlx::query_scalar("SELECT task_id FROM task_runs WHERE id = $1 FOR UPDATE").bind(run_id).fetch_optional(&mut *tx).await?;
        if task.as_deref() != Some(task_id.as_str()) || associated.as_deref().is_some_and(|old| old != run_id) { return Err(DbError::InvalidData("contradictory exact attempt/run association".into())); }
        sqlx::query("UPDATE task_attempts SET task_run_id = $1 WHERE id = $2").bind(run_id).bind(attempt_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO task_run_outcome_facts (task_run_id, attempt_seq, outcome) VALUES ($1, $2, 'observed') ON CONFLICT (task_run_id) DO NOTHING").bind(run_id).bind(seq).execute(&mut *tx).await?;
        let fact: TaskRunOutcomeFact = sqlx::query_as("SELECT task_run_id, attempt_seq, outcome, parked_reason, review_verdict, merge_queue_result, created_at, updated_at FROM task_run_outcome_facts WHERE task_run_id = $1").bind(run_id).fetch_one(&mut *tx).await?;
        if fact.attempt_seq != Some(seq) { return Err(DbError::InvalidData("contradictory immutable attempt ordinal".into())); }
        tx.commit().await?; Ok(fact)
    }
}
