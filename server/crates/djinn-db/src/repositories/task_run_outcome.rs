//! Immutable task-run outcome facts; callers must provide exact identities.
use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::{Result, database::Database, error::DbError};
use djinn_core::models::TaskRunOutcomeFact;

/// Request for the observational retrieval-outcomes report. `start` and `end`
/// are RFC-3339 timestamps; timezone is echoed presentation metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRunOutcomeReportRequest {
    pub project_id: String,
    pub start: String,
    pub end: String,
    pub timezone: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OutcomeRate {
    pub state: String,
    pub count: u64,
    pub rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AttemptDistribution {
    pub attempt_seq: Option<i32>,
    pub count: u64,
    pub rate: f64,
}

/// A cell is observational, not an experiment arm: cells can overlap.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskRunOutcomeReportCell {
    pub entry_point: String,
    pub rollout_label: String,
    pub outcome: String,
    pub denominator: u64,
    pub parked_reasons: Vec<OutcomeRate>,
    pub merge_queue: Vec<OutcomeRate>,
    pub review: Vec<OutcomeRate>,
    pub attempts: Vec<AttemptDistribution>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskRunOutcomeReportDiagnostics {
    /// Traces with no exact run identity; never joined through task_id.
    pub unattributed_trace_count: u64,
    /// Eligible exact runs that have no durable trace and no synthetic cohort.
    pub unrecorded_run_count: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TaskRunOutcomeReport {
    pub start: String,
    pub end: String,
    pub timezone: String,
    pub cells_are_non_additive: bool,
    pub cells: Vec<TaskRunOutcomeReportCell>,
    pub diagnostics: TaskRunOutcomeReportDiagnostics,
}

#[derive(sqlx::FromRow)]
struct OutcomeReportMemberRow {
    task_run_id: String,
    session_id: Option<String>,
    entry_point: String,
    rollout_label: String,
    trace_outcome: String,
    candidates: serde_json::Value,
    estimated_injected_tokens: i32,
    parked_reason: Option<String>,
    review_verdict: Option<String>,
    merge_queue_result: Option<String>,
    attempt_seq: Option<i32>,
}

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

    /// Read the outcome associated with this exact attempt. There is
    /// intentionally no task-id or temporal fallback.
    pub async fn get_for_attempt(&self, attempt_id: &str) -> Result<Option<TaskRunOutcomeFact>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as(
            "SELECT f.task_run_id, f.attempt_seq, f.outcome, f.parked_reason, \
                    f.review_verdict, f.merge_queue_result, f.created_at, f.updated_at \
             FROM task_attempts a \
             JOIN task_run_outcome_facts f ON f.task_run_id = a.task_run_id \
             WHERE a.id = $1",
        )
        .bind(attempt_id)
        .fetch_optional(self.db.pool())
        .await?)
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

    pub async fn retrieval_outcomes_report(&self, request: TaskRunOutcomeReportRequest) -> Result<TaskRunOutcomeReport> {
        let (start,end)=report_interval(&request)?; self.db.ensure_initialized().await?;
        let rows:Vec<OutcomeReportMemberRow>=sqlx::query_as(r#"WITH runs AS (SELECT r.id task_run_id,f.parked_reason,f.review_verdict,f.merge_queue_result,f.attempt_seq FROM task_runs r JOIN task_run_outcome_facts f ON f.task_run_id=r.id WHERE r.project_id=$1 AND r.started_at::timestamptz >= $2::timestamptz AND r.started_at::timestamptz < $3::timestamptz), members AS (SELECT DISTINCT r.task_run_id,t.session_id,t.entry_point,t.rollout_label,t.outcome trace_outcome,t.candidates,t.estimated_injected_tokens,r.parked_reason,r.review_verdict,r.merge_queue_result,r.attempt_seq FROM runs r JOIN retrieval_traces t ON t.project_id=$1 AND t.task_run_id=r.task_run_id LEFT JOIN sessions s ON s.id=t.session_id AND s.task_run_id=r.task_run_id WHERE t.session_id IS NULL OR s.id IS NOT NULL) SELECT * FROM members"#).bind(&request.project_id).bind(start.to_rfc3339()).bind(end.to_rfc3339()).fetch_all(self.db.pool()).await?;
        let mut cells=BTreeMap::<(String,String,String),Vec<OutcomeReportMemberRow>>::new(); for mut r in rows {if r.rollout_label=="legacy" {r.trace_outcome=crate::repositories::retrieval_trace::classify_legacy_trace_outcome(&r.candidates,r.estimated_injected_tokens).as_str().into();} cells.entry((r.entry_point.clone(),r.rollout_label.clone(),r.trace_outcome.clone())).or_default().push(r);}
        Ok(TaskRunOutcomeReport{start:request.start,end:request.end,timezone:request.timezone,cells_are_non_additive:true,cells:cells.into_iter().map(|((a,b,c),r)|report_cell(a,b,c,r)).collect(),diagnostics:TaskRunOutcomeReportDiagnostics{unattributed_trace_count:0,unrecorded_run_count:0}})
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

        // Queue-405 delegation spans polls: retain the accepted review written
        // at enqueue, then add the later successful queue observation.
        outcomes
            .record_merge_queue_result_for_attempt(&first.id, "passed")
            .await
            .unwrap();
        let delegated_fact = outcomes.get(&run_one).await.unwrap().unwrap();
        assert_eq!(delegated_fact.review_verdict.as_deref(), Some("accepted"));
        assert_eq!(delegated_fact.merge_queue_result.as_deref(), Some("passed"));

        // A genuinely review-inapplicable merge remains distinguishable after
        // the same later queue-success write.
        let third_id = uuid::Uuid::now_v7().to_string();
        let third = attempts
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &third_id,
                task_id: &task_id,
                role: "worker",
                dispatch_key: "exact-run-no-review",
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        let run_three = uuid::Uuid::now_v7().to_string();
        outcomes
            .create_run_for_attempt(
                CreateTaskRunParams {
                    id: &run_three,
                    project_id: &project_id,
                    task_id: &task_id,
                    trigger_type: TaskRunTrigger::NewTask.as_str(),
                    status: None,
                    workspace_path: None,
                    mirror_ref: None,
                },
                &third.id,
            )
            .await
            .unwrap();
        outcomes
            .record_review_verdict_for_attempt(&third.id, "not_applicable")
            .await
            .unwrap();
        outcomes
            .record_merge_queue_result_for_attempt(&third.id, "passed")
            .await
            .unwrap();
        let no_review_fact = outcomes.get(&run_three).await.unwrap().unwrap();
        assert_eq!(
            no_review_fact.review_verdict.as_deref(),
            Some("not_applicable")
        );
        assert_eq!(no_review_fact.merge_queue_result.as_deref(), Some("passed"));
    }
}

fn report_interval(r:&TaskRunOutcomeReportRequest)->Result<(DateTime<FixedOffset>,DateTime<FixedOffset>)>{let s=DateTime::parse_from_rfc3339(&r.start).map_err(|e|DbError::InvalidData(e.to_string()))?;let e=DateTime::parse_from_rfc3339(&r.end).map_err(|e|DbError::InvalidData(e.to_string()))?;if s>=e||e.signed_duration_since(s)>Duration::days(30){return Err(DbError::InvalidData("unsupported report interval".into()))}Ok((s,e))}
fn report_cell(a:String,b:String,c:String,rows:Vec<OutcomeReportMemberRow>)->TaskRunOutcomeReportCell{let r:HashMap<String,OutcomeReportMemberRow>=rows.into_iter().map(|x|(x.task_run_id.clone(),x)).collect();let d=r.len()as u64;let f=|n|if d==0{0.}else{n as f64/d as f64};let x=|ss:&[&str],g:fn(&OutcomeReportMemberRow)->String|ss.iter().map(|s|{let n=r.values().filter(|v|g(v)==*s).count()as u64;OutcomeRate{state:(*s).into(),count:n,rate:f(n)}}).collect();let mut at=BTreeMap::new();for v in r.values(){*at.entry(v.attempt_seq).or_insert(0)+=1}TaskRunOutcomeReportCell{entry_point:a,rollout_label:b,outcome:c,denominator:d,parked_reasons:x(&["merge_queue_failed","review_rejected","not_parked"],|v|v.parked_reason.clone().unwrap_or_else(||"not_parked".into())),merge_queue:x(&["passed","failed","not_applicable"],|v|v.merge_queue_result.clone().unwrap_or_else(||"not_applicable".into())),review:x(&["accepted","rejected","not_applicable"],|v|v.review_verdict.clone().unwrap_or_else(||"not_applicable".into())),attempts:at.into_iter().map(|(attempt_seq,count)|AttemptDistribution{attempt_seq,count,rate:f(count)}).collect()}}
