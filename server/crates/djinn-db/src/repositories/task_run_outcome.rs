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
    /// Create a run and attach the already allocated exact attempt in one transaction.
    /// A failed association rolls the run insertion back, so a fresh dispatch
    /// cannot leave an unattributed task run behind.
    pub async fn create_run_for_attempt(
        &self,
        params: crate::repositories::task_run::CreateTaskRunParams<'_>,
        attempt_id: &str,
    ) -> Result<djinn_core::models::TaskRunRecord> {
        self.db.ensure_initialized().await?;
        let status = params.status.unwrap_or("running");
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, workspace_path, mirror_ref) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(params.id).bind(params.project_id).bind(params.task_id).bind(params.trigger_type)
            .bind(status).bind(params.workspace_path).bind(params.mirror_ref).execute(&mut *tx).await?;
        let attempt: Option<(String, Option<String>, i32)> = sqlx::query_as(
            "SELECT task_id, task_run_id, attempt_seq FROM task_attempts WHERE id = $1 FOR UPDATE",
        )
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (attempt_task_id, associated_run_id, attempt_seq) =
            attempt.ok_or_else(|| DbError::InvalidData("task attempt does not exist".into()))?;
        if attempt_task_id != params.task_id
            || associated_run_id
                .as_deref()
                .is_some_and(|id| id != params.id)
        {
            return Err(DbError::InvalidData(
                "contradictory exact attempt/run association".into(),
            ));
        }
        sqlx::query("UPDATE task_attempts SET task_run_id = $1 WHERE id = $2")
            .bind(params.id)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO task_run_outcome_facts (task_run_id, attempt_seq, outcome) VALUES ($1, $2, 'observed')")
            .bind(params.id).bind(attempt_seq).execute(&mut *tx).await?;
        let run = sqlx::query_as("SELECT id, project_id, task_id, trigger_type, status, started_at, ended_at, workspace_path, mirror_ref FROM task_runs WHERE id = $1")
            .bind(params.id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(run)
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

    /// Resolve only through the supplied attempt's durable association. There
    /// is intentionally no task-id, current-state, or temporal fallback.
    async fn run_id_for_attempt(&self, attempt_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_scalar("SELECT task_run_id FROM task_attempts WHERE id = $1")
                .bind(attempt_id)
                .fetch_optional(self.db.pool())
                .await?
                .flatten(),
        )
    }

    pub async fn record_review_verdict_for_attempt(
        &self,
        attempt_id: &str,
        verdict: &str,
    ) -> Result<Option<TaskRunOutcomeFact>> {
        match self.run_id_for_attempt(attempt_id).await? {
            Some(run_id) => self.record_review_verdict(&run_id, verdict).await.map(Some),
            None => Ok(None),
        }
    }

    pub async fn record_merge_queue_result_for_attempt(
        &self,
        attempt_id: &str,
        result: &str,
    ) -> Result<Option<TaskRunOutcomeFact>> {
        match self.run_id_for_attempt(attempt_id).await? {
            Some(run_id) => self
                .record_merge_queue_result(&run_id, result)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub async fn record_parked_reason_for_attempt(
        &self,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Option<TaskRunOutcomeFact>> {
        match self.run_id_for_attempt(attempt_id).await? {
            Some(run_id) => self.record_parked_reason(&run_id, reason).await.map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;
    use djinn_core::models::TaskRunTrigger;

    use super::*;
    use crate::repositories::epic::EpicRepository;
    use crate::repositories::task_attempt::{CreateTaskAttemptParams, TaskAttemptRepository};
    use crate::repositories::task_run::CreateTaskRunParams;

    async fn create_task(db: &Database) -> (String, String) {
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .create("outcome facts", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, \
             issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs) \
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
        )
        .bind(&task_id)
        .bind(&epic.project_id)
        .bind(&short_id)
        .bind(&epic.id)
        .execute(db.pool())
        .await
        .unwrap();
        (epic.project_id, task_id)
    }

    #[tokio::test]
    async fn associates_each_run_with_its_supplied_attempt_and_ordinal() {
        let db = Database::open_in_memory().unwrap();
        let (project_id, task_id) = create_task(&db).await;
        let attempts = TaskAttemptRepository::new(db.clone());
        let outcomes = TaskRunOutcomeRepository::new(db.clone());

        let first_id = uuid::Uuid::now_v7().to_string();
        let first = attempts
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &first_id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "exact-run-1",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        let second_id = uuid::Uuid::now_v7().to_string();
        let second = attempts
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &second_id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "exact-run-2",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        let run_one = uuid::Uuid::now_v7().to_string();
        let run_two = uuid::Uuid::now_v7().to_string();
        outcomes
            .create_run_for_attempt(
                CreateTaskRunParams {
                    id: &run_one,
                    project_id: &project_id,
                    task_id: &task_id,
                    trigger_type: TaskRunTrigger::NewTask.as_str(),
                    status: None,
                    workspace_path: None,
                    mirror_ref: None,
                },
                &first.id,
            )
            .await
            .unwrap();
        outcomes
            .create_run_for_attempt(
                CreateTaskRunParams {
                    id: &run_two,
                    project_id: &project_id,
                    task_id: &task_id,
                    trigger_type: TaskRunTrigger::NewTask.as_str(),
                    status: None,
                    workspace_path: None,
                    mirror_ref: None,
                },
                &second.id,
            )
            .await
            .unwrap();

        // Each writer receives an authoritative attempt and follows only its
        // durable association. Retry is idempotent; contradiction is rejected.
        outcomes
            .record_review_verdict_for_attempt(&first.id, "accepted")
            .await
            .unwrap();
        outcomes
            .record_review_verdict_for_attempt(&first.id, "accepted")
            .await
            .unwrap();
        assert!(
            outcomes
                .record_review_verdict_for_attempt(&first.id, "rejected")
                .await
                .is_err()
        );
        outcomes
            .record_merge_queue_result_for_attempt(&second.id, "failed")
            .await
            .unwrap();
        outcomes
            .record_parked_reason_for_attempt(&second.id, "merge_queue_failed")
            .await
            .unwrap();

        assert_eq!(
            outcomes.get(&run_one).await.unwrap().unwrap().attempt_seq,
            Some(1)
        );
        assert_eq!(
            outcomes.get(&run_two).await.unwrap().unwrap().attempt_seq,
            Some(2)
        );
        let first_run: Option<String> =
            sqlx::query_scalar("SELECT task_run_id FROM task_attempts WHERE id = $1")
                .bind(&first.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let second_run: Option<String> =
            sqlx::query_scalar("SELECT task_run_id FROM task_attempts WHERE id = $1")
                .bind(&second.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(first_run.as_deref(), Some(run_one.as_str()));
        assert_eq!(second_run.as_deref(), Some(run_two.as_str()));
        let first_fact = outcomes.get(&run_one).await.unwrap().unwrap();
        let second_fact = outcomes.get(&run_two).await.unwrap().unwrap();
        assert_eq!(first_fact.review_verdict.as_deref(), Some("accepted"));
        assert!(first_fact.merge_queue_result.is_none());
        assert_eq!(second_fact.merge_queue_result.as_deref(), Some("failed"));
        assert_eq!(
            second_fact.parked_reason.as_deref(),
            Some("merge_queue_failed")
        );
    }
}
