use djinn_core::events::EventBus;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::database::Database;
use crate::error::{DbError as Error, DbResult as Result};

/// Database-backed diagnostics are bounded so a verbose provider error cannot
/// prevent persistence of the terminal usage and outcome.
pub const LLM_CALL_ATTEMPT_DIAGNOSTIC_MAX_CHARS: usize = 512;

fn bounded_diagnostic(diagnostic: Option<&str>) -> Option<String> {
    diagnostic.map(|value| {
        value
            .chars()
            .take(LLM_CALL_ATTEMPT_DIAGNOSTIC_MAX_CHARS)
            .collect()
    })
}

/// Append-only attributed LLM-call attempt outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmCallOutcome {
    Success,
    Timeout,
    InvalidPayload,
    ProviderError,
}

impl LlmCallOutcome {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Timeout => "timeout",
            Self::InvalidPayload => "invalid_payload",
            Self::ProviderError => "provider_error",
        }
    }
}

/// Create parameters for an attributed LLM-call attempt.
///
/// The row is inserted in a pending state (outcome = 'success' with
/// finalized_at = NULL) before provider I/O; the host finalizes it after
/// timeout, stream failure, or payload validation.
#[derive(Clone, Debug)]
pub struct CreateLlmCallAttemptParams<'a> {
    pub id: &'a str,
    pub project_id: &'a str,
    pub task_id: &'a str,
    pub task_run_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub created_by_user_id: Option<&'a str>,
    pub operation: &'a str,
    pub prompt_id: &'a str,
    pub model_id: &'a str,
    pub input_price_per_million_snapshot: Option<f64>,
    pub output_price_per_million_snapshot: Option<f64>,
    pub cache_read_price_per_million_snapshot: Option<f64>,
    pub cache_write_price_per_million_snapshot: Option<f64>,
}

/// Finalization parameters for an attributed LLM-call attempt.
#[derive(Clone, Debug)]
pub struct FinalizeLlmCallAttemptParams<'a> {
    pub id: &'a str,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub diagnostic: Option<&'a str>,
    pub outcome: LlmCallOutcome,
}

/// Persisted LLM-call attempt row.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LlmCallAttemptRecord {
    pub id: String,
    pub project_id: String,
    pub task_id: Option<String>,
    pub task_run_id: Option<String>,
    pub session_id: Option<String>,
    pub created_by_user_id: Option<String>,
    pub operation: String,
    pub prompt_id: String,
    pub model_id: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub input_price_per_million_snapshot: Option<f64>,
    pub output_price_per_million_snapshot: Option<f64>,
    pub cache_read_price_per_million_snapshot: Option<f64>,
    pub cache_write_price_per_million_snapshot: Option<f64>,
    pub cost_usd: Option<f64>,
    pub diagnostic: Option<String>,
    pub outcome: String,
    pub created_at: String,
    pub finalized_at: Option<String>,
}

impl LlmCallAttemptRecord {
    fn from_row(row: &sqlx::postgres::PgRow) -> Self {
        Self {
            id: row.get("id"),
            project_id: row.get("project_id"),
            task_id: row.get("task_id"),
            task_run_id: row.get("task_run_id"),
            session_id: row.get("session_id"),
            created_by_user_id: row.get("created_by_user_id"),
            operation: row.get("operation"),
            prompt_id: row.get("prompt_id"),
            model_id: row.get("model_id"),
            tokens_in: row.get("tokens_in"),
            tokens_out: row.get("tokens_out"),
            cache_read_tokens: row.get("cache_read_tokens"),
            cache_write_tokens: row.get("cache_write_tokens"),
            input_price_per_million_snapshot: row.get("input_price_per_million_snapshot"),
            output_price_per_million_snapshot: row.get("output_price_per_million_snapshot"),
            cache_read_price_per_million_snapshot: row.get("cache_read_price_per_million_snapshot"),
            cache_write_price_per_million_snapshot: row
                .get("cache_write_price_per_million_snapshot"),
            cost_usd: row.get("cost_usd"),
            diagnostic: row.get("diagnostic"),
            outcome: row.get("outcome"),
            created_at: row.get("created_at"),
            finalized_at: row.get("finalized_at"),
        }
    }
}

/// Repository for append-only attributed LLM-call attempts.
pub struct LlmCallAttemptRepository {
    db: Database,
    _events: EventBus,
}

impl LlmCallAttemptRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            db,
            _events: events,
        }
    }

    /// Insert a pending attempt before provider I/O.
    pub async fn create(
        &self,
        params: CreateLlmCallAttemptParams<'_>,
    ) -> Result<LlmCallAttemptRecord> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            r#"INSERT INTO llm_call_attempts
                (id, project_id, task_id, task_run_id, session_id, created_by_user_id,
                 operation, prompt_id, model_id,
                 input_price_per_million_snapshot,
                 output_price_per_million_snapshot,
                 cache_read_price_per_million_snapshot,
                 cache_write_price_per_million_snapshot,
                 outcome)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 'success')"#,
        )
        .bind(params.id)
        .bind(params.project_id)
        .bind(params.task_id)
        .bind(params.task_run_id)
        .bind(params.session_id)
        .bind(params.created_by_user_id)
        .bind(params.operation)
        .bind(params.prompt_id)
        .bind(params.model_id)
        .bind(params.input_price_per_million_snapshot)
        .bind(params.output_price_per_million_snapshot)
        .bind(params.cache_read_price_per_million_snapshot)
        .bind(params.cache_write_price_per_million_snapshot)
        .execute(self.db.pool())
        .await?;

        self.get(params.id).await?.ok_or_else(|| {
            Error::Internal("LlmCallAttemptRepository::create: row not found after insert".into())
        })
    }

    /// Finalize an attempt with observed usage, bounded diagnostic, and terminal outcome.
    ///
    /// Cost is computed using the session cost formula: `(tokens × snapshot rate) / 1_000_000`.
    pub async fn finalize(
        &self,
        params: FinalizeLlmCallAttemptParams<'_>,
    ) -> Result<LlmCallAttemptRecord> {
        self.db.ensure_initialized().await?;
        let diagnostic = bounded_diagnostic(params.diagnostic);

        let ti_f = params.tokens_in as f64;
        let to_f = params.tokens_out as f64;
        let cr_f = params.cache_read_tokens as f64;
        let cw_f = params.cache_write_tokens as f64;

        sqlx::query(
            r#"UPDATE llm_call_attempts
               SET tokens_in = $1,
                   tokens_out = $2,
                   cache_read_tokens = $3,
                   cache_write_tokens = $4,
                   cost_usd = CASE
                       WHEN input_price_per_million_snapshot IS NOT NULL
                        AND output_price_per_million_snapshot IS NOT NULL
                        AND cache_read_price_per_million_snapshot IS NOT NULL
                        AND cache_write_price_per_million_snapshot IS NOT NULL
                       THEN (
                           $5 * input_price_per_million_snapshot
                           + $6 * output_price_per_million_snapshot
                           + $7 * cache_read_price_per_million_snapshot
                           + $8 * cache_write_price_per_million_snapshot
                       ) / 1000000.0
                       ELSE NULL
                   END,
                   diagnostic = COALESCE($9, diagnostic),
                   outcome = $10,
                   finalized_at = to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $11"#,
        )
        .bind(params.tokens_in)
        .bind(params.tokens_out)
        .bind(params.cache_read_tokens)
        .bind(params.cache_write_tokens)
        .bind(ti_f)
        .bind(to_f)
        .bind(cr_f)
        .bind(cw_f)
        .bind(diagnostic)
        .bind(params.outcome.as_db_str())
        .bind(params.id)
        .execute(self.db.pool())
        .await?;

        self.get(params.id).await?.ok_or_else(|| {
            Error::Internal("LlmCallAttemptRepository::finalize: row not found after update".into())
        })
    }

    /// Fetch a single attempt by id.
    pub async fn get(&self, id: &str) -> Result<Option<LlmCallAttemptRecord>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"SELECT id, project_id, task_id, task_run_id, session_id, created_by_user_id,
                operation, prompt_id, model_id, tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens,
                input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_usd, diagnostic, outcome, created_at, finalized_at
             FROM llm_call_attempts WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| LlmCallAttemptRecord::from_row(&r)))
    }

    /// List attempts for a task, ordered by creation time and then id.
    ///
    /// `created_at` has millisecond precision, so concurrent attempts can share
    /// a timestamp. The id tie-breaker keeps ledger reads deterministic without
    /// relying on scheduler timing.
    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<LlmCallAttemptRecord>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query(
            r#"SELECT id, project_id, task_id, task_run_id, session_id, created_by_user_id,
                operation, prompt_id, model_id, tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens,
                input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_usd, diagnostic, outcome, created_at, finalized_at
             FROM llm_call_attempts WHERE task_id = $1 ORDER BY created_at, id"#,
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.iter().map(LlmCallAttemptRecord::from_row).collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    async fn seed_project(db: &Database, project_id: &str) {
        db.ensure_initialized().await.unwrap();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) \
             VALUES ($1, $2, 'test-owner', $2) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(project_id)
        .bind(format!("proj-{project_id}"))
        .execute(db.pool())
        .await
        .unwrap();
    }

    /// Seed the optional attribution parents used by the full round-trip test.
    /// The ledger deliberately retains these foreign keys so durable attempts
    /// cannot contain dangling task-run or session identifiers.
    async fn seed_task_run_and_session(
        db: &Database,
        project_id: &str,
        task_id: &str,
        task_run_id: &str,
        session_id: &str,
    ) {
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) \
             VALUES ($1, $2, $3, 'new_task', 'running')",
        )
        .bind(task_run_id)
        .bind(project_id)
        .bind(task_id)
        .execute(db.pool())
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO sessions \
             (id, project_id, task_id, task_run_id, model_id, agent_type, status) \
             VALUES ($1, $2, $3, $4, 'test-model', 'worker', 'running')",
        )
        .bind(session_id)
        .bind(project_id)
        .bind(task_id)
        .bind(task_run_id)
        .execute(db.pool())
        .await
        .unwrap();
    }

    async fn seed_task(db: &Database, task_id: &str, project_id: &str) {
        db.ensure_initialized().await.unwrap();
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, title, description, design, \
             issue_type, status, priority, acceptance_criteria, created_at, updated_at) \
             VALUES ($1, $2, $3, 'test', 'test', '', 'task', 'open', 0, '[]', \
             '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z') \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(task_id)
        .bind(project_id)
        // `tasks.short_id` is VARCHAR(32); the UUID task id is intentionally
        // stored only in `id`, not interpolated into this fixture value.
        .bind("test-task")
        .execute(db.pool())
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_finalize_success() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000001";
        let task_id = "019f4900-0000-7000-8000-000000000002";
        seed_project(&db, project_id).await;
        seed_task(&db, task_id, project_id).await;
        seed_task_run_and_session(&db, project_id, task_id, "run-1", "session-1").await;
        let repo = LlmCallAttemptRepository::new(db, EventBus::noop());

        let created = repo
            .create(CreateLlmCallAttemptParams {
                id: "call-1",
                project_id,
                task_id,
                task_run_id: Some("run-1"),
                session_id: Some("session-1"),
                created_by_user_id: None,
                operation: "memory_intent_planner",
                prompt_id: "memory-intent-planner-v1",
                model_id: "openai/gpt-4o-mini",
                input_price_per_million_snapshot: Some(2.5),
                output_price_per_million_snapshot: Some(10.0),
                cache_read_price_per_million_snapshot: Some(1.25),
                cache_write_price_per_million_snapshot: Some(1.25),
            })
            .await
            .unwrap();

        assert_eq!(created.outcome, "success");
        assert!(created.finalized_at.is_none());

        let finalized = repo
            .finalize(FinalizeLlmCallAttemptParams {
                id: "call-1",
                tokens_in: 1000,
                tokens_out: 500,
                cache_read_tokens: 100,
                cache_write_tokens: 50,
                diagnostic: None,
                outcome: LlmCallOutcome::Success,
            })
            .await
            .unwrap();

        assert_eq!(finalized.outcome, "success");
        assert!(finalized.finalized_at.is_some());
        let expected_cost =
            (1000.0 * 2.5 + 500.0 * 10.0 + 100.0 * 1.25 + 50.0 * 1.25) / 1_000_000.0;
        assert!((finalized.cost_usd.unwrap() - expected_cost).abs() < f64::EPSILON);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finalize_invalid_payload_with_usage() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000003";
        let task_id = "019f4900-0000-7000-8000-000000000004";
        seed_project(&db, project_id).await;
        seed_task(&db, task_id, project_id).await;
        let repo = LlmCallAttemptRepository::new(db, EventBus::noop());

        repo.create(CreateLlmCallAttemptParams {
            id: "call-2",
            project_id,
            task_id,
            task_run_id: None,
            session_id: None,
            created_by_user_id: None,
            operation: "memory_intent_planner",
            prompt_id: "memory-intent-planner-v1",
            model_id: "openai/gpt-4o-mini",
            input_price_per_million_snapshot: None,
            output_price_per_million_snapshot: None,
            cache_read_price_per_million_snapshot: None,
            cache_write_price_per_million_snapshot: None,
        })
        .await
        .unwrap();

        let finalized = repo
            .finalize(FinalizeLlmCallAttemptParams {
                id: "call-2",
                tokens_in: 100,
                tokens_out: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                diagnostic: Some("malformed JSON"),
                outcome: LlmCallOutcome::InvalidPayload,
            })
            .await
            .unwrap();

        assert_eq!(finalized.outcome, "invalid_payload");
        assert_eq!(finalized.tokens_in, 100);
        assert_eq!(finalized.tokens_out, 50);
        assert_eq!(finalized.diagnostic.as_deref(), Some("malformed JSON"));
        assert!(finalized.cost_usd.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_for_task_orders_by_created_at() {
        let db = test_db();
        let project_id = "019f4900-0000-7000-8000-000000000005";
        let task_id = "019f4900-0000-7000-8000-000000000006";
        seed_project(&db, project_id).await;
        seed_task(&db, task_id, project_id).await;
        let repo = LlmCallAttemptRepository::new(db, EventBus::noop());

        for i in 1..=3u32 {
            repo.create(CreateLlmCallAttemptParams {
                id: &format!("call-{i}"),
                project_id,
                task_id,
                task_run_id: None,
                session_id: None,
                created_by_user_id: None,
                operation: "memory_intent_planner",
                prompt_id: "memory-intent-planner-v1",
                model_id: "openai/gpt-4o-mini",
                input_price_per_million_snapshot: None,
                output_price_per_million_snapshot: None,
                cache_read_price_per_million_snapshot: None,
                cache_write_price_per_million_snapshot: None,
            })
            .await
            .unwrap();
        }

        let rows = repo.list_for_task(task_id).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "call-1");
        assert_eq!(rows[1].id, "call-2");
        assert_eq!(rows[2].id, "call-3");
    }
}
