// djinn:allow-oversize
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;

// ── Domain types ──────────────────────────────────────────────────────────────

/// The lifecycle state of a task arbitration row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArbitrationState {
    /// Arbiter dispatched, decision pending.
    Unconsumed,
    /// Arbiter returned a decision (approve / reject / park).
    Consumed,
    /// Arbiter or infra failure; terminal for this hold cycle.
    Failed,
}

impl ArbitrationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unconsumed => "unconsumed",
            Self::Consumed => "consumed",
            Self::Failed => "failed",
        }
    }

    pub fn parse_state(s: &str) -> Option<Self> {
        match s {
            "unconsumed" => Some(Self::Unconsumed),
            "consumed" => Some(Self::Consumed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Durable row for a single task arbitration keyed by `(task_id, hold_cycle)`.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaskArbitrationRecord {
    pub id: String,
    pub task_id: String,
    pub hold_cycle: i32,
    pub state: String,
    pub decision_failure_count: i32,
    pub infra_retry_count: i32,
    pub deadline_at: Option<String>,
    pub mirror_head_sha: Option<String>,
    pub github_head_sha: Option<String>,
    pub pr_url: Option<String>,
    /// JSON array of failing CI job ids.
    pub failing_ci_job_ids: serde_json::Value,
    /// Structured dossier JSON (opaque to the repo layer).
    pub dossier: Option<serde_json::Value>,
    /// Structured directive JSON (opaque to the repo layer).
    pub directive: Option<serde_json::Value>,
    pub verification_command: Option<String>,
    /// JSON array of excluded model ids.
    pub excluded_models: serde_json::Value,
    pub monitored_reopen_at: Option<String>,
    pub monitored_reopen_count: i32,
    /// True once the directive was injected into exactly one worker prompt.
    /// Subsequent worker prompts see this and return None.
    pub directive_injected: bool,
    pub consumed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl TaskArbitrationRecord {
    /// Parse the `state` column into the typed enum.
    pub fn arbitration_state(&self) -> Option<ArbitrationState> {
        ArbitrationState::parse_state(&self.state)
    }
}

// ── Input types ───────────────────────────────────────────────────────────────

/// Parameters for creating a new arbitration row.
pub struct CreateArbitrationParams<'a> {
    pub task_id: &'a str,
    pub hold_cycle: i32,
    pub deadline_at: Option<&'a str>,
    pub mirror_head_sha: Option<&'a str>,
    pub github_head_sha: Option<&'a str>,
    pub pr_url: Option<&'a str>,
    pub failing_ci_job_ids: &'a serde_json::Value,
    pub dossier: Option<&'a serde_json::Value>,
    pub directive: Option<&'a serde_json::Value>,
    pub verification_command: Option<&'a str>,
    pub excluded_models: &'a serde_json::Value,
}

/// Parameters for updating the dispatch ledger on an existing row.
pub struct UpdateDispatchLedgerParams<'a> {
    pub task_id: &'a str,
    pub hold_cycle: i32,
    pub mirror_head_sha: Option<&'a str>,
    pub github_head_sha: Option<&'a str>,
    pub pr_url: Option<&'a str>,
    pub failing_ci_job_ids: Option<&'a serde_json::Value>,
    pub dossier: Option<&'a serde_json::Value>,
    pub directive: Option<&'a serde_json::Value>,
    pub verification_command: Option<&'a str>,
    pub excluded_models: Option<&'a serde_json::Value>,
}

/// Outcome of a `try_create` call so the coordinator can distinguish
/// the four cases atomically.
#[derive(Clone, Debug)]
pub enum TryCreateResult {
    /// Row inserted — arbiter should be dispatched.
    Created(TaskArbitrationRecord),
    /// A row already exists for this `(task_id, hold_cycle)` and is still
    /// unconsumed. The arbiter is already in flight.
    AlreadyExistsUnconsumed(TaskArbitrationRecord),
    /// A row already exists and has been consumed. Re-entry path.
    AlreadyExistsConsumed(TaskArbitrationRecord),
    /// A row already exists in a terminal failed state.
    AlreadyExistsFailed(TaskArbitrationRecord),
}

// ── Repository ────────────────────────────────────────────────────────────────

pub struct TaskArbitrationRepository {
    db: Database,
}

impl TaskArbitrationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Atomically try to insert a new arbitration row. Returns the appropriate
    /// [`TryCreateResult`] variant so the coordinator can distinguish created
    /// from already-existing rows without a separate read.
    pub async fn try_create(&self, params: CreateArbitrationParams<'_>) -> Result<TryCreateResult> {
        self.db.ensure_initialized().await?;

        // Attempt INSERT. If the unique constraint fires, fall through to
        // the read path below.
        let id = uuid::Uuid::now_v7().to_string();
        let insert_result = sqlx::query(
            r#"INSERT INTO task_arbitrations (
                    id, task_id, hold_cycle, state,
                    deadline_at,
                    mirror_head_sha, github_head_sha, pr_url,
                    failing_ci_job_ids,
                    dossier, directive, verification_command, excluded_models
                ) VALUES ($1, $2, $3, 'unconsumed',
                    $4,
                    $5, $6, $7,
                    $8,
                    $9, $10, $11, $12)"#,
        )
        .bind(&id)
        .bind(params.task_id)
        .bind(params.hold_cycle)
        .bind(params.deadline_at)
        .bind(params.mirror_head_sha)
        .bind(params.github_head_sha)
        .bind(params.pr_url)
        .bind(params.failing_ci_job_ids)
        .bind(params.dossier)
        .bind(params.directive)
        .bind(params.verification_command)
        .bind(params.excluded_models)
        .execute(self.db.pool())
        .await;

        match insert_result {
            Ok(_) => {
                // Fresh row created — fetch it back.
                let record = self
                    .get_by_task_and_cycle(params.task_id, params.hold_cycle)
                    .await?
                    .expect("row just inserted must exist");
                Ok(TryCreateResult::Created(record))
            }
            Err(sqlx::Error::Database(db_err))
                if db_err
                    .constraint()
                    .map(|c| c == "uq_task_arbitrations_task_cycle")
                    .unwrap_or(false) =>
            {
                // Unique constraint violation — read the existing row.
                let existing = self
                    .get_by_task_and_cycle(params.task_id, params.hold_cycle)
                    .await?
                    .expect("unique violation implies row exists");
                match existing.arbitration_state() {
                    Some(ArbitrationState::Unconsumed) => {
                        Ok(TryCreateResult::AlreadyExistsUnconsumed(existing))
                    }
                    Some(ArbitrationState::Consumed) => {
                        Ok(TryCreateResult::AlreadyExistsConsumed(existing))
                    }
                    Some(ArbitrationState::Failed) => {
                        Ok(TryCreateResult::AlreadyExistsFailed(existing))
                    }
                    None => {
                        // Unknown state — treat as DB uncertainty.
                        Err(crate::Error::Internal(format!(
                            "task_arbitrations row has unknown state: {}",
                            existing.state
                        )))
                    }
                }
            }
            Err(e) => Err(crate::Error::from(e)),
        }
    }

    /// Read a single arbitration by its natural key.
    pub async fn get_by_task_and_cycle(
        &self,
        task_id: &str,
        hold_cycle: i32,
    ) -> Result<Option<TaskArbitrationRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, TaskArbitrationRecord>(ARBITRATION_SELECT_BY_TASK_CYCLE)
                .bind(task_id)
                .bind(hold_cycle)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    /// Read the most recent arbitration for a task (highest hold_cycle).
    pub async fn get_latest_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<TaskArbitrationRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, TaskArbitrationRecord>(ARBITRATION_SELECT_LATEST_FOR_TASK)
                .bind(task_id)
                .fetch_optional(self.db.pool())
                .await?,
        )
    }

    /// Resolve the current hold cycle for `task_id`.
    ///
    /// Returns `(current_hold_cycle, existing_unconsumed_record)`:
    /// - If the latest arbitration row is `unconsumed`, the current cycle is
    ///   that row's `hold_cycle` and the record is returned.
    /// - If the latest row is `consumed` or `failed`, or no row exists, the
    ///   current cycle is `latest.hold_cycle + 1` (or `0` if there are no rows)
    ///   and no record is returned.
    pub async fn resolve_current_hold_cycle(
        &self,
        task_id: &str,
    ) -> Result<(i32, Option<TaskArbitrationRecord>)> {
        let latest = self.get_latest_for_task(task_id).await?;
        match latest {
            Some(record) => match record.arbitration_state() {
                Some(ArbitrationState::Unconsumed) => Ok((record.hold_cycle, Some(record))),
                _ => Ok((record.hold_cycle.saturating_add(1), None)),
            },
            None => Ok((0, None)),
        }
    }

    /// Mark an unconsumed arbitration as consumed (arbiter decision received).
    /// Returns `true` if the transition was applied, `false` if the row was
    /// already consumed or missing.
    pub async fn mark_consumed(&self, task_id: &str, hold_cycle: i32) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_MARK_CONSUMED)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Mark an unconsumed arbitration as failed (terminal for this cycle).
    /// Returns `true` if the transition was applied.
    pub async fn mark_failed(&self, task_id: &str, hold_cycle: i32) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_MARK_FAILED)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Increment the decision failure count for observability.
    pub async fn increment_decision_failure(&self, task_id: &str, hold_cycle: i32) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_INCREMENT_DECISION_FAILURE)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Increment the infra retry count for observability.
    pub async fn increment_infra_retry(&self, task_id: &str, hold_cycle: i32) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_INCREMENT_INFRA_RETRY)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update the dispatch ledger fields and optional structured payloads.
    /// Only non-NULL fields in `params` are written.
    pub async fn update_dispatch_ledger(
        &self,
        params: UpdateDispatchLedgerParams<'_>,
    ) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_UPDATE_DISPATCH_LEDGER)
            .bind(params.mirror_head_sha)
            .bind(params.github_head_sha)
            .bind(params.pr_url)
            .bind(params.failing_ci_job_ids)
            .bind(params.dossier)
            .bind(params.directive)
            .bind(params.verification_command)
            .bind(params.excluded_models)
            .bind(params.task_id)
            .bind(params.hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Record a monitored-reopen attempt.
    pub async fn record_monitored_reopen(&self, task_id: &str, hold_cycle: i32) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_RECORD_MONITORED_REOPEN)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Atomically mark that the directive has been injected into exactly one
    /// worker prompt.  Returns `true` when the flag transitioned from `false`
    /// to `true` (first call); returns `false` when it was already `true`
    /// (second worker prompt on re-entry).  This is the one-shot guard: the
    /// caller calls `load_arbiter_directive`, and only injects when this
    /// method returns `true`.
    ///
    /// The UPDATE's `WHERE directive_injected = false` clause makes it
    /// idempotent: concurrent calls race but only one wins.
    pub async fn mark_directive_injected(&self, task_id: &str, hold_cycle: i32) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_MARK_DIRECTIVE_INJECTED)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Mark a monitored-reopen attempt as complete.  Called on any terminal
    /// outcome of the monitored worker attempt: worker submit, reviewer
    /// rejection, CI failure, worker failure, or no-eligible-model.
    /// This transitions the arbitration row to `consumed` (terminal for this
    /// hold cycle) so re-entry cannot trigger a second arbiter or worker retry.
    ///
    /// Returns `true` if the row was consumed by this call (first terminal
    /// outcome), `false` if already consumed/failed.
    pub async fn complete_monitored_reopen(&self, task_id: &str, hold_cycle: i32) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_COMPLETE_MONITORED_REOPEN)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Force the raw arbitration state for repository-boundary regression tests.
    ///
    /// This intentionally does not validate `state`: callers use it to simulate
    /// malformed durable rows while keeping direct SQL access inside `djinn-db`.
    #[doc(hidden)]
    pub async fn force_state_for_testing(
        &self,
        task_id: &str,
        hold_cycle: i32,
        state: &str,
    ) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(ARBITRATION_FORCE_STATE_FOR_TESTING)
            .bind(state)
            .bind(task_id)
            .bind(hold_cycle)
            .execute(self.db.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List all arbitrations for a task ordered by hold_cycle.
    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<TaskArbitrationRecord>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, TaskArbitrationRecord>(ARBITRATION_SELECT_LIST_FOR_TASK)
                .bind(task_id)
                .fetch_all(self.db.pool())
                .await?,
        )
    }
}

// ── SQL constants ─────────────────────────────────────────────────────────────

const ARBITRATION_SELECT_BY_TASK_CYCLE: &str = r#"
    SELECT
        id,
        task_id,
        hold_cycle,
        state,
        decision_failure_count,
        infra_retry_count,
        deadline_at,
        mirror_head_sha,
        github_head_sha,
        pr_url,
        failing_ci_job_ids,
        dossier,
        directive,
        verification_command,
        excluded_models,
        monitored_reopen_at,
        monitored_reopen_count,
        directive_injected,
        consumed_at,
        created_at,
        updated_at
    FROM task_arbitrations
    WHERE task_id = $1 AND hold_cycle = $2
"#;

const ARBITRATION_SELECT_LATEST_FOR_TASK: &str = r#"
    SELECT
        id,
        task_id,
        hold_cycle,
        state,
        decision_failure_count,
        infra_retry_count,
        deadline_at,
        mirror_head_sha,
        github_head_sha,
        pr_url,
        failing_ci_job_ids,
        dossier,
        directive,
        verification_command,
        excluded_models,
        monitored_reopen_at,
        monitored_reopen_count,
        directive_injected,
        consumed_at,
        created_at,
        updated_at
    FROM task_arbitrations
    WHERE task_id = $1
    ORDER BY hold_cycle DESC
    LIMIT 1
"#;

const ARBITRATION_SELECT_LIST_FOR_TASK: &str = r#"
    SELECT
        id,
        task_id,
        hold_cycle,
        state,
        decision_failure_count,
        infra_retry_count,
        deadline_at,
        mirror_head_sha,
        github_head_sha,
        pr_url,
        failing_ci_job_ids,
        dossier,
        directive,
        verification_command,
        excluded_models,
        monitored_reopen_at,
        monitored_reopen_count,
        directive_injected,
        consumed_at,
        created_at,
        updated_at
    FROM task_arbitrations
    WHERE task_id = $1
    ORDER BY hold_cycle ASC
"#;

const ARBITRATION_MARK_CONSUMED: &str = r#"
    UPDATE task_arbitrations
    SET state = 'consumed',
        consumed_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
        updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $1 AND hold_cycle = $2 AND state = 'unconsumed'
"#;

const ARBITRATION_MARK_FAILED: &str = r#"
    UPDATE task_arbitrations
    SET state = 'failed',
        updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $1 AND hold_cycle = $2 AND state = 'unconsumed'
"#;

const ARBITRATION_INCREMENT_DECISION_FAILURE: &str = r#"
    UPDATE task_arbitrations
    SET decision_failure_count = decision_failure_count + 1,
        updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $1 AND hold_cycle = $2
"#;

const ARBITRATION_INCREMENT_INFRA_RETRY: &str = r#"
    UPDATE task_arbitrations
    SET infra_retry_count = infra_retry_count + 1,
        updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $1 AND hold_cycle = $2
"#;

const ARBITRATION_UPDATE_DISPATCH_LEDGER: &str = r#"
    UPDATE task_arbitrations
    SET mirror_head_sha      = COALESCE($1, mirror_head_sha),
        github_head_sha      = COALESCE($2, github_head_sha),
        pr_url               = COALESCE($3, pr_url),
        failing_ci_job_ids   = COALESCE($4, failing_ci_job_ids),
        dossier              = COALESCE($5, dossier),
        directive            = COALESCE($6, directive),
        verification_command = COALESCE($7, verification_command),
        excluded_models      = COALESCE($8, excluded_models),
        updated_at           = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $9 AND hold_cycle = $10
"#;

const ARBITRATION_RECORD_MONITORED_REOPEN: &str = r#"
    UPDATE task_arbitrations
    SET monitored_reopen_at    = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
        monitored_reopen_count = monitored_reopen_count + 1,
        updated_at             = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $1 AND hold_cycle = $2
"#;

const ARBITRATION_MARK_DIRECTIVE_INJECTED: &str = r#"
    UPDATE task_arbitrations
    SET directive_injected = true,
        updated_at         = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $1 AND hold_cycle = $2 AND directive_injected = false
"#;

const ARBITRATION_COMPLETE_MONITORED_REOPEN: &str = r#"
    UPDATE task_arbitrations
    SET state       = 'consumed',
        consumed_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
        updated_at  = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $1 AND hold_cycle = $2 AND state = 'unconsumed'
"#;

const ARBITRATION_FORCE_STATE_FOR_TESTING: &str = r#"
    UPDATE task_arbitrations
    SET state = $1,
        updated_at = to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
    WHERE task_id = $2 AND hold_cycle = $3
"#;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;

    use super::*;
    use crate::repositories::epic::EpicRepository;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Create a task via raw SQL (no TaskRepository dep), returns (project_id, task_id).
    async fn create_task(db: &Database, bus: EventBus) -> (String, String) {
        let epic_repo = EpicRepository::new(db.clone(), bus);
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();

        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
            task_id,
            epic.project_id,
            short_id,
            epic.id
        )
        .execute(db.pool())
        .await
        .unwrap();

        (epic.project_id, task_id)
    }

    fn sample_params<'a>(
        task_id: &'a str,
        hold_cycle: i32,
        failing_ci_job_ids: &'a serde_json::Value,
        excluded_models: &'a serde_json::Value,
    ) -> CreateArbitrationParams<'a> {
        CreateArbitrationParams {
            task_id,
            hold_cycle,
            deadline_at: Some("2026-12-31T23:59:59.000Z"),
            mirror_head_sha: Some("abc123def456"),
            github_head_sha: Some("789012fed345"),
            pr_url: Some("https://github.com/org/repo/pull/42"),
            failing_ci_job_ids,
            dossier: None,
            directive: None,
            verification_command: Some("cargo test -p foo"),
            excluded_models,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_create_inserts_unconsumed_row() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        let result = repo
            .try_create(sample_params(
                &task_id,
                1,
                &serde_json::json!([]),
                &serde_json::json!([]),
            ))
            .await
            .unwrap();

        match &result {
            TryCreateResult::Created(record) => {
                assert_eq!(record.task_id, task_id);
                assert_eq!(record.hold_cycle, 1);
                assert_eq!(record.state, "unconsumed");
                assert_eq!(record.mirror_head_sha.as_deref(), Some("abc123def456"));
                assert_eq!(record.github_head_sha.as_deref(), Some("789012fed345"));
                assert_eq!(
                    record.pr_url.as_deref(),
                    Some("https://github.com/org/repo/pull/42")
                );
                assert_eq!(
                    record.verification_command.as_deref(),
                    Some("cargo test -p foo")
                );
                assert!(record.consumed_at.is_none());
                assert!(!record.created_at.is_empty());
            }
            other => panic!("expected Created, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_create_uniqueness_conflict_unconsumed() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();

        // Second call with same (task_id, hold_cycle) → AlreadyExistsUnconsumed.
        let result = repo
            .try_create(sample_params(
                &task_id,
                1,
                &serde_json::json!([]),
                &serde_json::json!([]),
            ))
            .await
            .unwrap();
        match &result {
            TryCreateResult::AlreadyExistsUnconsumed(record) => {
                assert_eq!(record.task_id, task_id);
                assert_eq!(record.hold_cycle, 1);
                assert_eq!(record.state, "unconsumed");
            }
            other => panic!("expected AlreadyExistsUnconsumed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_create_uniqueness_conflict_consumed() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        repo.mark_consumed(&task_id, 1).await.unwrap();

        let result = repo
            .try_create(sample_params(
                &task_id,
                1,
                &serde_json::json!([]),
                &serde_json::json!([]),
            ))
            .await
            .unwrap();
        match &result {
            TryCreateResult::AlreadyExistsConsumed(record) => {
                assert_eq!(record.state, "consumed");
            }
            other => panic!("expected AlreadyExistsConsumed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn try_create_uniqueness_conflict_failed() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        repo.mark_failed(&task_id, 1).await.unwrap();

        let result = repo
            .try_create(sample_params(
                &task_id,
                1,
                &serde_json::json!([]),
                &serde_json::json!([]),
            ))
            .await
            .unwrap();
        match &result {
            TryCreateResult::AlreadyExistsFailed(record) => {
                assert_eq!(record.state, "failed");
            }
            other => panic!("expected AlreadyExistsFailed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn different_hold_cycles_do_not_conflict() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        let r1 = repo
            .try_create(sample_params(
                &task_id,
                1,
                &serde_json::json!([]),
                &serde_json::json!([]),
            ))
            .await
            .unwrap();
        let r2 = repo
            .try_create(sample_params(
                &task_id,
                2,
                &serde_json::json!([]),
                &serde_json::json!([]),
            ))
            .await
            .unwrap();

        assert!(matches!(r1, TryCreateResult::Created(_)));
        assert!(matches!(r2, TryCreateResult::Created(_)));

        let list = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_consumed_transitions_and_sets_timestamp() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();

        let before = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert!(before.consumed_at.is_none());

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let applied = repo.mark_consumed(&task_id, 1).await.unwrap();
        assert!(applied);

        let after = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, "consumed");
        assert!(after.consumed_at.is_some());
        assert!(after.updated_at >= before.updated_at);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_consumed_noop_on_already_consumed() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        assert!(repo.mark_consumed(&task_id, 1).await.unwrap());
        // Second mark is a no-op (WHERE state = 'unconsumed' won't match).
        assert!(!repo.mark_consumed(&task_id, 1).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_failed_transitions_state() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        assert!(repo.mark_failed(&task_id, 1).await.unwrap());

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn increment_decision_failure_counter() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();

        for expected in 1..=3 {
            assert!(repo.increment_decision_failure(&task_id, 1).await.unwrap());
            let record = repo
                .get_by_task_and_cycle(&task_id, 1)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(record.decision_failure_count, expected);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn increment_infra_retry_counter() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();

        assert!(repo.increment_infra_retry(&task_id, 1).await.unwrap());
        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.infra_retry_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_ledger_round_trip() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        // Create with initial ledger values.
        let dossier = serde_json::json!({
            "failure_summary": "clippy errors in module X",
            "attempt_count": 2
        });
        let directive = serde_json::json!({
            "action": "approve",
            "reason": "fix applied in abc123"
        });
        let failing_jobs = serde_json::json!([12345, 67890]);
        let excluded = serde_json::json!(["gpt-4o-mini", "kimi-for-coding"]);

        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: Some("2026-12-31T23:59:59.000Z"),
            mirror_head_sha: Some("mirror_sha_initial"),
            github_head_sha: Some("github_sha_initial"),
            pr_url: Some("https://github.com/org/repo/pull/1"),
            failing_ci_job_ids: &failing_jobs,
            dossier: Some(&dossier),
            directive: Some(&directive),
            verification_command: Some("cargo test"),
            excluded_models: &excluded,
        })
        .await
        .unwrap();

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.mirror_head_sha.as_deref(),
            Some("mirror_sha_initial")
        );
        assert_eq!(
            record.github_head_sha.as_deref(),
            Some("github_sha_initial")
        );
        assert_eq!(
            record.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/1")
        );
        assert_eq!(record.failing_ci_job_ids, serde_json::json!([12345, 67890]));
        assert_eq!(record.dossier.as_ref().unwrap(), &dossier);
        assert_eq!(record.directive.as_ref().unwrap(), &directive);
        assert_eq!(record.verification_command.as_deref(), Some("cargo test"));
        assert_eq!(record.excluded_models, excluded);

        // Update ledger with new values.
        let new_jobs = serde_json::json!([99999]);
        repo.update_dispatch_ledger(UpdateDispatchLedgerParams {
            task_id: &task_id,
            hold_cycle: 1,
            mirror_head_sha: Some("mirror_sha_updated"),
            github_head_sha: Some("github_sha_updated"),
            pr_url: Some("https://github.com/org/repo/pull/2"),
            failing_ci_job_ids: Some(&new_jobs),
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: None,
        })
        .await
        .unwrap();

        let updated = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.mirror_head_sha.as_deref(),
            Some("mirror_sha_updated")
        );
        assert_eq!(
            updated.github_head_sha.as_deref(),
            Some("github_sha_updated")
        );
        assert_eq!(
            updated.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/2")
        );
        assert_eq!(updated.failing_ci_job_ids, serde_json::json!([99999]));
        // Unchanged fields preserved.
        assert_eq!(updated.dossier.as_ref().unwrap(), &dossier);
        assert_eq!(updated.directive.as_ref().unwrap(), &directive);
        assert_eq!(updated.verification_command.as_deref(), Some("cargo test"));
        assert_eq!(updated.excluded_models, excluded);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_latest_returns_highest_hold_cycle() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        repo.try_create(sample_params(
            &task_id,
            3,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        repo.try_create(sample_params(
            &task_id,
            2,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();

        let latest = repo.get_latest_for_task(&task_id).await.unwrap().unwrap();
        assert_eq!(latest.hold_cycle, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_latest_returns_none_for_unknown_task() {
        let db = test_db();
        let repo = TaskArbitrationRepository::new(db);

        let result = repo
            .get_latest_for_task("00000000-0000-0000-0000-000000000000")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitored_reopen_lifecycle() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();

        let before = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert!(before.monitored_reopen_at.is_none());
        assert_eq!(before.monitored_reopen_count, 0);
        assert!(!before.directive_injected);

        repo.record_monitored_reopen(&task_id, 1).await.unwrap();
        let after = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert!(after.monitored_reopen_at.is_some());
        assert_eq!(after.monitored_reopen_count, 1);

        repo.record_monitored_reopen(&task_id, 1).await.unwrap();
        let twice = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(twice.monitored_reopen_count, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jsonb_dossier_and_directive_round_trip() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        let dossier = serde_json::json!({
            "summary": "Three consecutive clippy failures in djinn-coordinator",
            "failing_files": ["retry.rs", "dispatch.rs"],
            "ci_run_id": 123456,
            "nested": {
                "attempt": 2,
                "model": "claude-opus-4-20250514"
            }
        });
        let directive = serde_json::json!({
            "verdict": "reject",
            "instructions": "Rewrite retry.rs dispatch path",
            "exclusions": ["gpt-4o-mini"]
        });

        let empty_arr = serde_json::json!([]);
        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty_arr,
            dossier: Some(&dossier),
            directive: Some(&directive),
            verification_command: None,
            excluded_models: &empty_arr,
        })
        .await
        .unwrap();

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.dossier.as_ref().unwrap(), &dossier);
        assert_eq!(record.directive.as_ref().unwrap(), &directive);

        // Verify nested structure survives.
        let d = record.dossier.as_ref().unwrap();
        assert_eq!(d["nested"]["attempt"], 2);
        assert_eq!(d["nested"]["model"], "claude-opus-4-20250514");
        assert_eq!(d["ci_run_id"], 123456);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_consumed_does_not_affect_failed_row() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        repo.mark_failed(&task_id, 1).await.unwrap();

        // mark_consumed's WHERE requires state = 'unconsumed', so it's a no-op.
        assert!(!repo.mark_consumed(&task_id, 1).await.unwrap());
        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "failed");
    }

    // ── zkk9: directive one-shot injection and completion tests ─────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_directive_injected_one_shot_guard() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();

        // Before mark, directive_injected is false.
        let before = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert!(!before.directive_injected);

        // First mark succeeds (first worker prompt).
        let first = repo.mark_directive_injected(&task_id, 1).await.unwrap();
        assert!(first, "first mark_directive_injected should return true");

        // Second mark returns false (second worker prompt — re-entry).
        let second = repo.mark_directive_injected(&task_id, 1).await.unwrap();
        assert!(
            !second,
            "second mark_directive_injected should return false (one-shot guard)"
        );

        // Verify the flag is true and stays true.
        let after = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert!(after.directive_injected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_monitored_reopen_transitions_to_consumed() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        repo.record_monitored_reopen(&task_id, 1).await.unwrap();

        // Complete the monitored reopen.
        let applied = repo.complete_monitored_reopen(&task_id, 1).await.unwrap();
        assert!(applied, "complete_monitored_reopen should return true");

        let after = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, "consumed");
        assert!(after.consumed_at.is_some());

        // Idempotent: a second call is a no-op.
        let second = repo.complete_monitored_reopen(&task_id, 1).await.unwrap();
        assert!(
            !second,
            "complete_monitored_reopen on consumed row returns false"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_monitored_reopen_no_op_on_failed_row() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!([]),
        ))
        .await
        .unwrap();
        repo.record_monitored_reopen(&task_id, 1).await.unwrap();
        repo.mark_failed(&task_id, 1).await.unwrap();

        let applied = repo.complete_monitored_reopen(&task_id, 1).await.unwrap();
        assert!(
            !applied,
            "complete_monitored_reopen on failed row should return false"
        );

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "failed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_current_hold_cycle_returns_directive_injected_flag() {
        // Verify that the directive_injected field is correctly returned by
        // resolve_current_hold_cycle. This exercises the full SELECT path.
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!(["model-a", "model-b"]),
        ))
        .await
        .unwrap();
        repo.record_monitored_reopen(&task_id, 1).await.unwrap();

        let (_, Some(record)) = repo.resolve_current_hold_cycle(&task_id).await.unwrap() else {
            panic!("expected unconsumed record");
        };
        assert_eq!(record.monitored_reopen_count, 1);
        assert!(!record.directive_injected);
        assert_eq!(
            record.excluded_models,
            serde_json::json!(["model-a", "model-b"])
        );

        // Mark injected — re-resolve should show directive_injected = true.
        repo.mark_directive_injected(&task_id, 1).await.unwrap();
        let (_, Some(record2)) = repo.resolve_current_hold_cycle(&task_id).await.unwrap() else {
            panic!("expected unconsumed record after mark");
        };
        assert!(record2.directive_injected);
    }

    // ── zkk9 round 3: full lifecycle regression tests ──────────────────────

    /// End-to-end monitored reopen lifecycle at the repository level:
    /// 1. Create row (unconsumed, monitored_reopen_count = 0)
    /// 2. record_monitored_reopen → count = 1, row still unconsumed
    /// 3. mark_directive_injected → first call true (one-shot), second false
    /// 4. Row still unconsumed after injection (directive persists)
    /// 5. complete_monitored_reopen → consumed
    /// 6. No second arbiter/worker cycle: resolve_current_hold_cycle returns
    ///    (next_cycle, None) after consumption — no unconsumed row to re-enter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitored_reopen_full_lifecycle_no_double_reentry() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        // 1. Create unconsumed row.
        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!(["bad-model"]),
        ))
        .await
        .unwrap();

        // 2. Start monitored reopen — row stays unconsumed.
        repo.record_monitored_reopen(&task_id, 1).await.unwrap();
        let after_start = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_start.monitored_reopen_count, 1);
        assert_eq!(after_start.state, "unconsumed");
        assert!(!after_start.directive_injected);

        // resolve_current_hold_cycle still returns the unconsumed row.
        let (cycle, unconsumed) = repo.resolve_current_hold_cycle(&task_id).await.unwrap();
        assert_eq!(cycle, 1);
        assert!(unconsumed.is_some());

        // 3. First directive injection wins (one-shot).
        let first_inject = repo.mark_directive_injected(&task_id, 1).await.unwrap();
        assert!(first_inject, "first mark_directive_injected should win");

        // Second directive injection loses (re-entry).
        let second_inject = repo.mark_directive_injected(&task_id, 1).await.unwrap();
        assert!(
            !second_inject,
            "second mark_directive_injected must lose (one-shot guard)"
        );

        // 4. Row still unconsumed after injection — directive persists for
        //    coordinator exclude_models enforcement.
        let after_inject = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_inject.state, "unconsumed");
        assert!(after_inject.directive_injected);
        assert_eq!(after_inject.monitored_reopen_count, 1);

        // 5. Complete the monitored reopen on worker terminal outcome.
        let completed = repo.complete_monitored_reopen(&task_id, 1).await.unwrap();
        assert!(completed, "complete_monitored_reopen should succeed");

        let after_complete = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_complete.state, "consumed");

        // 6. No second arbiter/worker cycle: resolve_current_hold_cycle
        //    returns (next_cycle, None) — no unconsumed row to re-enter.
        let (cycle2, unconsumed2) = repo.resolve_current_hold_cycle(&task_id).await.unwrap();
        assert_eq!(
            cycle2, 2,
            "after consumption, next hold cycle is incremented"
        );
        assert!(
            unconsumed2.is_none(),
            "no unconsumed row remains — no second arbiter/worker retry"
        );
    }

    /// No-eligible-model scenario: when exclude_models eliminates all worker
    /// models, the coordinator calls complete_monitored_reopen and the row
    /// transitions to consumed.  No second arbiter dispatch is possible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_eligible_model_completes_monitored_reopen() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        // Create row with excluded models that would eliminate all workers.
        repo.try_create(sample_params(
            &task_id,
            1,
            &serde_json::json!([]),
            &serde_json::json!(["model-a", "model-b", "model-c"]),
        ))
        .await
        .unwrap();
        repo.record_monitored_reopen(&task_id, 1).await.unwrap();

        // Coordinator detects no eligible model and completes the monitored
        // reopen.  The row transitions to consumed — no second arbiter cycle.
        let completed = repo.complete_monitored_reopen(&task_id, 1).await.unwrap();
        assert!(completed);

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "consumed");

        // resolve_current_hold_cycle returns (next_cycle, None).
        let (cycle, unconsumed) = repo.resolve_current_hold_cycle(&task_id).await.unwrap();
        assert_eq!(cycle, 2);
        assert!(unconsumed.is_none());
    }

    /// Directive injection is one-shot: after mark_directive_injected, a
    /// second resolve_current_hold_cycle returns the row with
    /// directive_injected == true.  The prompt_context layer checks this flag
    /// and returns None for the second worker prompt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn directive_injection_consumed_after_first_worker_prompt() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);

        let directive = serde_json::json!({"decision": "reopen", "directive": "Fix the bug"});
        let empty_arr = serde_json::json!([]);

        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty_arr,
            dossier: None,
            directive: Some(&directive),
            verification_command: Some("cargo test"),
            excluded_models: &empty_arr,
        })
        .await
        .unwrap();

        repo.record_monitored_reopen(&task_id, 1).await.unwrap();

        // First worker prompt: directive_injected is false → inject.
        let (_, Some(rec1)) = repo.resolve_current_hold_cycle(&task_id).await.unwrap() else {
            panic!("expected unconsumed record");
        };
        assert!(!rec1.directive_injected);
        assert!(rec1.monitored_reopen_count >= 1);

        // Atomically claim the injection.
        let claimed = repo.mark_directive_injected(&task_id, 1).await.unwrap();
        assert!(claimed);

        // Second worker prompt (re-entry): directive_injected is true →
        // load_arbiter_directive returns None (no duplicate injection).
        let (_, Some(rec2)) = repo.resolve_current_hold_cycle(&task_id).await.unwrap() else {
            panic!("expected unconsumed record");
        };
        assert!(
            rec2.directive_injected,
            "second worker prompt must see directive_injected == true"
        );
        assert_eq!(
            rec2.state, "unconsumed",
            "row still unconsumed until terminal"
        );
    }

    /// Infra-class failures before a decision increment only infra_retry_count
    /// and do NOT increment decision_failure_count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infra_failure_does_not_increment_decision_failure_count() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);
        let empty = serde_json::json!([]);

        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &empty,
        })
        .await
        .unwrap();

        // Simulate 3 infra failures.
        for _ in 0..3 {
            repo.increment_infra_retry(&task_id, 1).await.unwrap();
        }

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.infra_retry_count, 3);
        assert_eq!(
            record.decision_failure_count, 0,
            "infra failures must NOT increment decision_failure_count"
        );
    }

    /// No-valid-decision failures increment decision_failure_count.  Two
    /// such failures reach the cap; a third call still increments the
    /// counter (the cap enforcement is in the caller, not the SQL).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decision_failure_count_increments_and_can_be_capped() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);
        let empty = serde_json::json!([]);

        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &empty,
        })
        .await
        .unwrap();

        // First no-decision failure.
        repo.increment_decision_failure(&task_id, 1).await.unwrap();
        let r1 = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r1.decision_failure_count, 1);
        assert_eq!(r1.state, "unconsumed");

        // Second no-decision failure — cap reached.
        repo.increment_decision_failure(&task_id, 1).await.unwrap();
        let r2 = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r2.decision_failure_count, 2);

        // Mark failed at cap — transitions to terminal failed state.
        assert!(repo.mark_failed(&task_id, 1).await.unwrap());
        let r3 = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r3.state, "failed");
    }

    /// Mixed infra and decision failures accumulate independently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn infra_and_decision_failures_accumulate_independently() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);
        let empty = serde_json::json!([]);

        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: None,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &empty,
        })
        .await
        .unwrap();

        // 2 infra failures, 1 decision failure.
        repo.increment_infra_retry(&task_id, 1).await.unwrap();
        repo.increment_infra_retry(&task_id, 1).await.unwrap();
        repo.increment_decision_failure(&task_id, 1).await.unwrap();

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.infra_retry_count, 2);
        assert_eq!(record.decision_failure_count, 1);
        assert_eq!(record.state, "unconsumed");
    }

    /// mark_failed on an unconsumed row with expired deadline transitions
    /// to failed, demonstrating the terminal state for deadline auto-park.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadline_expiry_mark_failed_transitions_to_failed() {
        let db = test_db();
        let (_proj, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = TaskArbitrationRepository::new(db);
        let empty = serde_json::json!([]);

        // Create with a past deadline.
        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: Some("2020-01-01T00:00:00.000Z"),
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: &empty,
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &empty,
        })
        .await
        .unwrap();

        let record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, "unconsumed");
        assert_eq!(
            record.deadline_at.as_deref(),
            Some("2020-01-01T00:00:00.000Z")
        );

        // Simulate deadline auto-park: mark failed and update dossier.
        repo.mark_failed(&task_id, 1).await.unwrap();

        let dossier = serde_json::json!({
            "kind": "arbiter_deadline_expired",
            "summary": "Arbitration deadline expired",
            "hold_cycle": 1,
            "deadline_at": "2020-01-01T00:00:00.000Z",
        });
        repo.update_dispatch_ledger(UpdateDispatchLedgerParams {
            task_id: &task_id,
            hold_cycle: 1,
            mirror_head_sha: None,
            github_head_sha: None,
            pr_url: None,
            failing_ci_job_ids: None,
            dossier: Some(&dossier),
            directive: None,
            verification_command: None,
            excluded_models: None,
        })
        .await
        .unwrap();

        let final_record = repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_record.state, "failed");
        let stored_dossier = final_record.dossier.unwrap();
        assert_eq!(stored_dossier["kind"], "arbiter_deadline_expired");
        assert_eq!(stored_dossier["deadline_at"], "2020-01-01T00:00:00.000Z");
    }
}
