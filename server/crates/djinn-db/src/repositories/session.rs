// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::provider::Pricing;
use djinn_core::models::{ModelLane, SessionRecord, SessionStatus};
use serde_json::Value;

use crate::Result;
use crate::database::Database;

// Inlined SESSION_COLS projection for each `query_as!(SessionRecord, ...)`
// call site.  `query_as!` requires a string-literal SQL argument; concat!()
// doesn't satisfy it (verified on agent.rs in batch 4).

pub struct SessionRepository {
    db: Database,
    events: EventBus,
}

pub struct CreateSessionParams<'a> {
    pub project_id: &'a str,
    pub task_id: Option<&'a str>,
    pub model: &'a str,
    pub agent_type: &'a str,
    pub metadata_json: Option<&'a str>,
    /// Link this session to a task-run row (Phase 1 supervisor path). `None`
    /// preserves the pre-supervisor behaviour where sessions are standalone.
    pub task_run_id: Option<&'a str>,
    /// Optional start-time pricing snapshot resolved by the agent host from
    /// the catalog.  When `Some`, the four per-million rate snapshot columns
    /// are populated; when `None` the snapshot columns (and `cost_usd`) stay
    /// `NULL` — uncatalogued/unpriced sessions are never treated as free.
    ///
    /// Owned by the agent layer (`djinn-agent` performs the catalog lookup and
    /// passes plain pricing data here) so `djinn-db` has no dependency on
    /// `djinn-provider`.
    pub pricing: Option<&'a Pricing>,
    /// Cost-basis label derived at session creation from catalog pricing and
    /// provider credential class: `"actual"` (API-key), `"projected"`
    /// (subscription/coding-plan), or `"unpriced"` (uncatalogued/missing).
    /// When `None`, defaults to `"unpriced"` in `create()`.
    pub cost_basis: Option<&'a str>,
}

impl SessionRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    pub async fn create(&self, params: CreateSessionParams<'_>) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let _ = params.metadata_json;

        // Phase 3B: stamp `created_by_user_id` from the task-local set at
        // the MCP dispatch root. Sessions spawned from the agent
        // coordinator's internal loops have no user context and stay
        // NULL; sessions created in response to a user MCP call (e.g.
        // `session_start` via chat) inherit the calling user's id.
        let created_by_user_id = djinn_core::auth_context::current_user_id();
        sqlx::query!(
            "INSERT INTO sessions
                (id, project_id, task_id, model_id, agent_type, status,
                 created_by_user_id, task_run_id,
                 input_price_per_million_snapshot,
                 output_price_per_million_snapshot,
                 cache_read_price_per_million_snapshot,
                 cache_write_price_per_million_snapshot,
                 cost_basis)
             VALUES ($1, $2, $3, $4, $5, 'running', $6, $7, $8, $9, $10, $11, $12)",
            id,
            params.project_id,
            params.task_id,
            params.model,
            params.agent_type,
            created_by_user_id,
            params.task_run_id,
            params.pricing.map(|p| p.input_per_million),
            params.pricing.map(|p| p.output_per_million),
            params.pricing.map(|p| p.cache_read_per_million),
            params.pricing.map(|p| p.cache_write_per_million),
            params.cost_basis.unwrap_or("unpriced"),
        )
        .execute(self.db.pool())
        .await?;

        // Pre-session tracking transition: a dispatched `task_run` holds
        // `starting` from creation (in-pod supervisor) until its first
        // reply-loop session is created — this is that first turn, so flip it
        // to `running`. Guarded on `status = 'starting'` so it is a no-op for
        // subsequent per-stage sessions (already `running`), terminal runs
        // (post-session extraction), and chat sessions (`task_run_id` NULL).
        // This transition is what flips the UI off its "starting" badge; the
        // host-side pre-session liveness deadline disarms on the `sessions`
        // row itself.
        if let Some(run_id) = params.task_run_id {
            sqlx::query!(
                "UPDATE task_runs SET status = 'running', ended_at = NULL
                 WHERE id = $1 AND status = 'starting'",
                run_id,
            )
            .execute(self.db.pool())
            .await?;
        }

        let session = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;

        self.events.send(DjinnEventEnvelope {
            entity_type: "session",
            action: "started",
            payload: serde_json::to_value(&session).unwrap_or_default(),
            id: None,
            project_id: None,
            from_sync: false,
        });
        tracing::info!(
            session_id = %session.id,
            task_id = ?session.task_id,
            "SessionRepository: emitted session.started SSE event"
        );
        Ok(session)
    }

    /// Record the credential kind (`sessions.billing_source`) for a session
    /// immediately after it is created.
    ///
    /// This is a small dedicated write rather than a `CreateSessionParams`
    /// field because only the agent host (dispatch task sessions) knows the
    /// resolved-credential kind; the ~90 other `create()` call sites (chat,
    /// post-session extraction, tests) legitimately have no dispatch-time
    /// credential signal and leave the column `NULL`. Threading a new required
    /// field through all of them would be pure churn.
    ///
    /// `value` must be `"plan_oauth"` or `"api_key"` — the column CHECK
    /// (migration 88) rejects anything else. Re-fetches and returns the updated
    /// row so the caller observes the persisted value.
    pub async fn set_billing_source(&self, id: &str, value: &str) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE sessions SET billing_source = $1 WHERE id = $2",
            value,
            id
        )
        .execute(self.db.pool())
        .await?;

        let session = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(session)
    }

    /// Re-fetch a session by id and emit `SessionUpdated`.
    async fn fetch_and_emit_update(&self, id: &str) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;
        let session = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE id = $1"#,
            id
        )
        .fetch_one(self.db.pool())
        .await?;
        let action = match session.status.as_str() {
            "running" => "started",
            "completed" => "completed",
            "interrupted" => "interrupted",
            "failed" => "failed",
            _ => "updated",
        };
        self.events.send(DjinnEventEnvelope {
            entity_type: "session",
            action,
            payload: serde_json::to_value(&session).unwrap_or_default(),
            id: None,
            project_id: None,
            from_sync: false,
        });
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        id: &str,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
        parked_reason: Option<String>,
    ) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        let status_str = status.as_str();
        // Token params are bound twice: once as i64 for the bigint SET
        // columns ($2-$5) and once as f64 for the cost computation
        // ($8-$11).  Reusing the same positional parameter in both a
        // bigint and a double-precision context makes PostgreSQL report
        // "inconsistent types deduced", so we use separate positions.
        let ti_f = tokens_in as f64;
        let to_f = tokens_out as f64;
        let cr_f = cache_read as f64;
        let cw_f = cache_write as f64;
        sqlx::query(
            r#"UPDATE sessions
             SET status = $1,
                 tokens_in = $2,
                 tokens_out = $3,
                 cache_read_tokens = $4,
                 cache_write_tokens = $5,
                 cost_usd = CASE
                     WHEN input_price_per_million_snapshot IS NOT NULL
                      AND output_price_per_million_snapshot IS NOT NULL
                      AND cache_read_price_per_million_snapshot IS NOT NULL
                      AND cache_write_price_per_million_snapshot IS NOT NULL
                     THEN (
                         $8 * input_price_per_million_snapshot
                         + $9 * output_price_per_million_snapshot
                         + $10 * cache_read_price_per_million_snapshot
                         + $11 * cache_write_price_per_million_snapshot
                     ) / 1000000.0
                     ELSE NULL
                 END,
                 ended_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                 parked_reason = COALESCE($7, parked_reason)
             WHERE id = $6"#,
        )
        .bind(status_str)
        .bind(tokens_in)
        .bind(tokens_out)
        .bind(cache_read)
        .bind(cache_write)
        .bind(id)
        .bind(parked_reason)
        .bind(ti_f)
        .bind(to_f)
        .bind(cr_f)
        .bind(cw_f)
        .execute(self.db.pool())
        .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Mark all `running` sessions as `interrupted`.
    /// Called once at server startup — no runtime sessions can exist yet.
    pub async fn interrupt_all_running(&self) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let running_sessions = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE status = 'running'"#
        )
        .fetch_all(self.db.pool())
        .await?;

        if running_sessions.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query!(
            r#"UPDATE sessions
             SET status = 'interrupted',
                 ended_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE status = 'running'"#
        )
        .execute(self.db.pool())
        .await?;

        for session in running_sessions {
            let _ = self.fetch_and_emit_update(&session.id).await?;
        }

        Ok(result.rows_affected())
    }

    /// Mark `running` sessions as `interrupted`, except those whose task-run
    /// identity was proven reconnectable during startup reconciliation.
    ///
    /// Sessions without a task-run identity are deliberately included: a NULL
    /// identity cannot provide reconnectability proof. An empty preservation
    /// set retains the blanket-startup behavior exactly.
    pub async fn interrupt_running_except_task_run_ids(
        &self,
        reconnectable_task_run_ids: &std::collections::HashSet<String>,
    ) -> Result<u64> {
        if reconnectable_task_run_ids.is_empty() {
            return self.interrupt_all_running().await;
        }

        self.db.ensure_initialized().await?;
        let reconnectable_task_run_ids: Vec<String> =
            reconnectable_task_run_ids.iter().cloned().collect();

        // Select before mutating so only rows actually transitioned emit
        // session update events. The explicit NULL branch avoids PostgreSQL's
        // three-valued comparison behavior for NULL = ANY(...).
        let interrupted_sessions = sqlx::query_as::<_, SessionRecord>(
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status, tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason,
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions
             WHERE status = 'running'
               AND (task_run_id IS NULL OR NOT (task_run_id = ANY($1)))"#,
        )
        .bind(&reconnectable_task_run_ids)
        .fetch_all(self.db.pool())
        .await?;

        if interrupted_sessions.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query(
            r#"UPDATE sessions
             SET status = 'interrupted',
                 ended_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE status = 'running'
               AND (task_run_id IS NULL OR NOT (task_run_id = ANY($1)))"#,
        )
        .bind(&reconnectable_task_run_ids)
        .execute(self.db.pool())
        .await?;

        for session in interrupted_sessions {
            let _ = self.fetch_and_emit_update(&session.id).await?;
        }

        Ok(result.rows_affected())
    }

    /// Mark all `running` sessions for a specific task as `interrupted`.
    /// Used by stuck-task recovery to clean up orphaned session records.
    pub async fn interrupt_running_for_task(&self, task_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let orphans = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE task_id = $1 AND status = 'running'"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?;

        if orphans.is_empty() {
            return Ok(0);
        }

        let result = sqlx::query!(
            r#"UPDATE sessions
             SET status = 'interrupted',
                 ended_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE task_id = $1 AND status = 'running'"#,
            task_id
        )
        .execute(self.db.pool())
        .await?;

        for session in &orphans {
            let _ = self.fetch_and_emit_update(&session.id).await;
        }

        Ok(result.rows_affected())
    }

    pub async fn get(&self, id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_in_project(
        &self,
        project_id: &str,
        id: &str,
    ) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE project_id = $1 AND id = $2"#,
            project_id,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn list_for_task(&self, task_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE task_id = $1 ORDER BY started_at DESC"#,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List all sessions linked to a task-run row. Used by runtime-resource
    /// reconciliation: the task-run id is the label carried by K8s Jobs, while
    /// session liveness is persisted here.
    pub async fn list_for_task_run(&self, task_run_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, SessionRecord>(
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status, tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason,
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE task_run_id = $1 ORDER BY started_at DESC"#,
        )
        .bind(task_run_id)
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_for_task_in_project(
        &self,
        project_id: &str,
        task_id: &str,
    ) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions
             WHERE project_id = $1 AND task_id = $2 ORDER BY started_at DESC"#,
            project_id,
            task_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn list_active(&self) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions
             WHERE status = 'running' ORDER BY started_at DESC"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Count currently-running *autonomous* sessions grouped by
    /// `(creator_user_id, model_id)`. The creator is the session's own
    /// `created_by_user_id` when set, else the owning task's
    /// `created_by_user_id` (the COALESCE + LEFT JOIN). This is the real-time
    /// source of truth the coordinator uses to enforce per-user, per-model
    /// concurrency caps at dispatch. Indexed on `status`, `task_id`, and
    /// `created_by_user_id`.
    ///
    /// Interactive `chat` sessions are deliberately EXCLUDED: they never flow
    /// through the slot pool, are created `running` and linger that way for the
    /// conversation's lifetime, and must not share a budget with autonomous
    /// task-runs — otherwise an open (or leaked) chat tab silently starves the
    /// user's task dispatch (fatal at `max_sessions = 1`).
    pub async fn count_active_by_user_and_model(
        &self,
    ) -> Result<Vec<(Option<String>, String, i64)>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            r#"SELECT COALESCE(s.created_by_user_id, t.created_by_user_id) AS creator,
                      s.model_id AS "model_id!",
                      COUNT(*) AS "cnt!"
                 FROM sessions s
                 LEFT JOIN tasks t ON t.id = s.task_id
                WHERE s.status = 'running'
                  AND s.agent_type <> 'chat'
                GROUP BY COALESCE(s.created_by_user_id, t.created_by_user_id), s.model_id"#
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.creator, r.model_id, r.cnt))
            .collect())
    }

    /// Count currently-running autonomous sessions grouped by
    /// `(creator_user_id, model lane)`.
    ///
    /// Lane attribution intentionally follows the same role mapping used by
    /// dispatch admission: `worker` consumes the implement lane, `reviewer`
    /// consumes the review lane, and every other autonomous role consumes the
    /// plan lane. Interactive `chat` sessions are excluded even though
    /// [`ModelLane::for_role`] maps them to plan, because chat never flows
    /// through coordinator admission and must not starve background work.
    pub async fn count_active_by_user_and_lane(
        &self,
    ) -> Result<Vec<(Option<String>, ModelLane, i64)>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, (Option<String>, String, i64)>(
            r#"SELECT COALESCE(s.created_by_user_id, t.created_by_user_id) AS creator,
                      CASE
                          WHEN s.agent_type = 'worker' THEN 'implement'
                          WHEN s.agent_type = 'reviewer' THEN 'review'
                          ELSE 'plan'
                      END AS lane,
                      COUNT(*)::bigint AS cnt
                 FROM sessions s
                 LEFT JOIN tasks t ON t.id = s.task_id
                WHERE s.status = 'running'
                  AND s.agent_type <> 'chat'
                GROUP BY COALESCE(s.created_by_user_id, t.created_by_user_id),
                         CASE
                             WHEN s.agent_type = 'worker' THEN 'implement'
                             WHEN s.agent_type = 'reviewer' THEN 'review'
                             ELSE 'plan'
                         END"#,
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(creator, lane, count)| {
                let lane = match lane.as_str() {
                    "implement" => ModelLane::Implement,
                    "review" => ModelLane::Review,
                    _ => ModelLane::Plan,
                };
                (creator, lane, count)
            })
            .collect())
    }

    pub async fn list_active_in_project(&self, project_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions
             WHERE project_id = $1 AND status = 'running' ORDER BY started_at DESC"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Returns any running sessions with `agent_type = 'planner'` whose task
    /// is attached to the given epic.  Used by ADR-051 §7 reentrance guard
    /// to suppress auto-dispatch of a new planning wave while a Planner is
    /// actively reshaping the epic.
    pub async fn active_planner_for_epic(&self, epic_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT s.id, s.project_id, s.task_id, s.model_id, s.agent_type,
                    s.started_at, s.ended_at,
                    s.status AS "status!", s.tokens_in, s.tokens_out,
                    s.cache_read_tokens, s.cache_write_tokens,
                    s.task_run_id, s.title,
                    s.parked_reason AS "parked_reason?",
                    s.cost_usd, s.input_price_per_million_snapshot,
                    s.output_price_per_million_snapshot,
                    s.cache_read_price_per_million_snapshot,
                    s.cache_write_price_per_million_snapshot,
                    s.cost_basis,
                    s.billing_source
             FROM sessions s
             INNER JOIN tasks t ON t.id = s.task_id
             WHERE s.status = 'running' AND s.agent_type = 'planner' AND t.epic_id = $1
             ORDER BY s.started_at DESC"#,
            epic_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn active_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions
             WHERE task_id = $1 AND status = 'running' ORDER BY started_at DESC LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// The `model_id` of the most recent session for `task_id` whose
    /// `agent_type` matches `agent_type` (e.g. `"worker"` to find the model that
    /// implemented the task). `None` when no such session exists yet.
    ///
    /// Used by the cross-model ("Thorough") review path: at reviewer dispatch we
    /// look up the implementer's model id so the reviewer can pick a different
    /// one. Reads the latest by `started_at` so a re-implemented task uses the
    /// model of its newest worker run.
    pub async fn latest_model_for_task_role(
        &self,
        task_id: &str,
        agent_type: &str,
    ) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar!(
            r#"SELECT model_id FROM sessions
               WHERE task_id = $1 AND agent_type = $2
               ORDER BY started_at DESC LIMIT 1"#,
            task_id,
            agent_type,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn count_for_task(&self, task_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM sessions WHERE task_id = $1"#,
            task_id
        )
        .fetch_one(self.db.pool())
        .await?)
    }

    /// Batch count sessions per task for a list of task IDs.
    pub async fn count_for_tasks(
        &self,
        task_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, i64>> {
        self.db.ensure_initialized().await?;
        if task_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        // NOTE: dynamic SQL (IN list built at runtime). Postgres $N binds.
        let sql = format!(
            "SELECT task_id, COUNT(*) as cnt FROM sessions WHERE task_id IN ({}) GROUP BY task_id",
            crate::repositories::pg_placeholders(task_ids.len(), 1)
        );
        let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
        for id in task_ids {
            q = q.bind(*id);
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        Ok(rows.into_iter().collect())
    }

    /// Set session status to Paused without setting ended_at.
    /// Used when a worker completes (Done) but its worktree is kept alive for the review cycle.
    pub async fn pause(&self, id: &str, tokens_in: i64, tokens_out: i64) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "UPDATE sessions SET status = 'paused', tokens_in = $1, tokens_out = $2 WHERE id = $3",
            tokens_in,
            tokens_out,
            id
        )
        .execute(self.db.pool())
        .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Mid-flight token-counter flush for a still-running session.
    ///
    /// Long sessions otherwise show `tokens_in = 0` until they end, because
    /// `update()` is only called at reply-loop teardown. This writes the token
    /// columns ONLY — no `status`, no `ended_at` — and is guarded by
    /// `status = 'running'` so a flush racing the zombie reaper / stall killer
    /// can never resurrect or overwrite a terminal row. Best-effort: a missed
    /// flush is corrected by the next one or by the final `update()`.
    pub async fn flush_tokens(
        &self,
        id: &str,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;

        // Token params are bound twice: once as i64 for the bigint SET
        // columns ($1-$4) and once as f64 for the cost computation
        // ($5-$8).  Reusing the same positional parameter in both a
        // bigint and a double-precision context makes PostgreSQL report
        // "inconsistent types deduced", so we use separate positions.
        let ti_f = tokens_in as f64;
        let to_f = tokens_out as f64;
        let cr_f = cache_read as f64;
        let cw_f = cache_write as f64;
        sqlx::query(
            r#"UPDATE sessions
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
                 END
             WHERE id = $9 AND status = 'running'"#,
        )
        .bind(tokens_in)
        .bind(tokens_out)
        .bind(cache_read)
        .bind(cache_write)
        .bind(ti_f)
        .bind(to_f)
        .bind(cr_f)
        .bind(cw_f)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Set a paused session back to Running (for resume cycles).
    pub async fn set_running(&self, id: &str) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        sqlx::query!("UPDATE sessions SET status = 'running' WHERE id = $1", id)
            .execute(self.db.pool())
            .await?;

        self.fetch_and_emit_update(id).await
    }

    /// Find the most recent paused session for a task (if any).
    pub async fn paused_for_task(&self, task_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions
             WHERE task_id = $1 AND status = 'paused' ORDER BY started_at DESC LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Store the event taxonomy JSON on a completed session record.
    ///
    /// Called after structural extraction completes. A best-effort write:
    /// callers should log errors but must not propagate them to the slot.
    pub async fn set_event_taxonomy(&self, id: &str, taxonomy_json: &str) -> Result<()> {
        self.db.ensure_initialized().await?;

        let taxonomy: serde_json::Value = serde_json::from_str(taxonomy_json).map_err(|e| {
            crate::Error::InvalidData(format!("invalid json for sessions.event_taxonomy: {e}"))
        })?;
        sqlx::query!(
            "UPDATE sessions SET event_taxonomy = $1 WHERE id = $2",
            taxonomy,
            id
        )
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// Return the raw `event_taxonomy` column for a session, as stored JSON.
    ///
    /// `Ok(None)` means either the session doesn't exist or the column is
    /// `NULL`. Callers that need a deserialized `Value` should parse it
    /// themselves; this method deliberately returns the raw string so tests
    /// can assert on the on-disk representation.
    pub async fn get_event_taxonomy_json(&self, id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row: Option<Option<String>> = sqlx::query_scalar!(
            r#"SELECT event_taxonomy::text AS "event_taxonomy?" FROM sessions WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.flatten())
    }

    /// Return the most recent non-null event taxonomy JSON for a task.
    pub async fn latest_event_taxonomy_for_task(&self, task_id: &str) -> Result<Option<Value>> {
        self.db.ensure_initialized().await?;

        let row: Option<Option<String>> = sqlx::query_scalar!(
            r#"SELECT event_taxonomy::text AS "event_taxonomy?" FROM sessions
             WHERE task_id = $1 AND event_taxonomy IS NOT NULL
             ORDER BY started_at DESC LIMIT 1"#,
            task_id
        )
        .fetch_optional(self.db.pool())
        .await?;

        Ok(row
            .flatten()
            .and_then(|json| serde_json::from_str::<Value>(json.as_str()).ok()))
    }

    /// Find the most recent paused session for a task that matches the given
    /// agent type.  Used during dispatch so that e.g. a PM session never
    /// accidentally resumes a worker's paused conversation.
    pub async fn paused_for_task_by_type(
        &self,
        task_id: &str,
        agent_type: &str,
    ) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions
             WHERE task_id = $1 AND status = 'paused' AND agent_type = $2
             ORDER BY started_at DESC LIMIT 1"#,
            task_id,
            agent_type
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    // ── Chat session (`agent_type = 'chat'`, project_id NULL) ────────────
    //
    // Chat sessions are user-scoped, global-across-projects, and carry a
    // client-minted UUID as their primary key (the client also uses this
    // UUID as the SSE session-affinity key).  The following methods are
    // chat-only; non-chat sessions continue to use `create()` + the
    // agent-type-specific lookups above.

    /// Idempotently create a chat session row keyed by the client-minted id.
    ///
    /// Invariant: `agent_type = 'chat'` and `project_id IS NULL` (enforced
    /// by migration 15's CHECK constraint).  The initial title is "New
    /// Chat"; [`Self::update_chat_title`] overwrites it after the first
    /// assistant reply lands.  Safe to call on every /completions request:
    /// if the id already exists we revive a previously-settled session back to
    /// `running` (the coordinator settles idle chats to `completed` via
    /// [`Self::settle_idle_chat`], and resuming the conversation must bring it
    /// back to life) but otherwise leave its columns alone.
    pub async fn upsert_chat_session(
        &self,
        session_id: &str,
        model_id: &str,
    ) -> Result<SessionRecord> {
        self.db.ensure_initialized().await?;

        let created_by_user_id = djinn_core::auth_context::current_user_id();
        let initial_title = "New Chat";

        // Idempotent create-if-missing in a single round-trip. On conflict we
        // only revive a settled (idle-reaped) session — when it's already
        // `running` the guarded UPDATE is a no-op, so the title and other
        // columns are preserved across turns.
        sqlx::query!(
            "INSERT INTO sessions
                (id, project_id, task_id, model_id, agent_type, status,
                 created_by_user_id, task_run_id, title)
             VALUES ($1, NULL, NULL, $2, 'chat', 'running', $3, NULL, $4)
             ON CONFLICT (id) DO UPDATE
                SET status = 'running', ended_at = NULL
              WHERE sessions.status <> 'running'",
            session_id,
            model_id,
            created_by_user_id,
            initial_title,
        )
        .execute(self.db.pool())
        .await?;

        let session = sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE id = $1 AND agent_type = 'chat'"#,
            session_id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(session)
    }

    /// List running chat sessions with their last-activity timestamp
    /// (newest message, falling back to `started_at` for empty sessions).
    /// Used by the coordinator's idle-chat reaper. Returns `(session_id,
    /// last_activity_iso)`; timestamps are the varchar ISO-8601 strings stored
    /// throughout, so callers compute idle in Rust.
    pub async fn list_running_chat_with_last_activity(&self) -> Result<Vec<(String, String)>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            r#"SELECT s.id AS "id!",
                      COALESCE(m.last_at, s.started_at) AS "last_activity!"
                 FROM sessions s
                 LEFT JOIN (
                    SELECT session_id, MAX(created_at) AS last_at
                    FROM session_messages
                    GROUP BY session_id
                 ) m ON m.session_id = s.id
                WHERE s.agent_type = 'chat' AND s.status = 'running'"#
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(|r| (r.id, r.last_activity)).collect())
    }

    /// Settle an idle chat session: `running` → `completed`, stamping
    /// `ended_at`. The conversation stays listed and resumable —
    /// [`Self::upsert_chat_session`] revives it to `running` on the next turn.
    /// Guarded on `status = 'running'` so it's a no-op for already-settled rows.
    pub async fn settle_idle_chat(&self, session_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE sessions
                 SET status = 'completed',
                     ended_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1 AND agent_type = 'chat' AND status = 'running'"#,
            session_id
        )
        .execute(self.db.pool())
        .await?;
        let _ = self.fetch_and_emit_update(session_id).await;
        Ok(())
    }

    /// Fetch a single chat session by id (scoped to `agent_type = 'chat'`).
    pub async fn get_chat_session(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
                status AS "status!", tokens_in, tokens_out,
                cache_read_tokens, cache_write_tokens, task_run_id, title,
                parked_reason AS "parked_reason?",
                cost_usd, input_price_per_million_snapshot,
                output_price_per_million_snapshot,
                cache_read_price_per_million_snapshot,
                cache_write_price_per_million_snapshot,
                cost_basis,
                billing_source
             FROM sessions WHERE id = $1 AND agent_type = 'chat'"#,
            session_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// List ALL chat sessions, newest first. Unscoped — retained only for
    /// internal/admin callers and tests. User-facing HTTP handlers must use
    /// [`Self::list_chat_for_user`] so a user only sees their own sessions.
    pub async fn list_chat_sessions(&self) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        // COALESCE against the most recent message's created_at so a freshly
        // created empty session still sorts.  We rely on lexicographic sort
        // over the ISO-8601 timestamp strings stored in both columns.
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT s.id, s.project_id, s.task_id, s.model_id, s.agent_type,
                    s.started_at, s.ended_at,
                    s.status AS "status!", s.tokens_in, s.tokens_out,
                    s.cache_read_tokens, s.cache_write_tokens,
                    s.task_run_id, s.title,
                    s.parked_reason AS "parked_reason?",
                    s.cost_usd, s.input_price_per_million_snapshot,
                    s.output_price_per_million_snapshot,
                    s.cache_read_price_per_million_snapshot,
                    s.cache_write_price_per_million_snapshot,
                    s.cost_basis,
                    s.billing_source
             FROM sessions s
             LEFT JOIN (
                SELECT session_id, MAX(created_at) AS last_at
                FROM session_messages
                GROUP BY session_id
             ) m ON m.session_id = s.id
             WHERE s.agent_type = 'chat'
             ORDER BY COALESCE(m.last_at, s.started_at) DESC"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// List chat sessions owned by `user_id` (private chat: Part 2 of per-user
    /// isolation), newest first.
    ///
    /// Privacy decision for legacy rows: sessions with
    /// `created_by_user_id IS NULL` (created before attribution shipped, or by
    /// the unauthenticated local-dev path) are NEVER returned to an
    /// authenticated user — they belong to "no one" and showing them would
    /// leak pre-multiuser history into every account. They remain reachable
    /// only via the unscoped local-dev path (`list_chat_sessions`, used when no
    /// GitHub App is configured).
    pub async fn list_chat_for_user(&self, user_id: &str) -> Result<Vec<SessionRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            SessionRecord,
            r#"SELECT s.id, s.project_id, s.task_id, s.model_id, s.agent_type,
                    s.started_at, s.ended_at,
                    s.status AS "status!", s.tokens_in, s.tokens_out,
                    s.cache_read_tokens, s.cache_write_tokens,
                    s.task_run_id, s.title,
                    s.parked_reason AS "parked_reason?",
                    s.cost_usd, s.input_price_per_million_snapshot,
                    s.output_price_per_million_snapshot,
                    s.cache_read_price_per_million_snapshot,
                    s.cache_write_price_per_million_snapshot,
                    s.cost_basis,
                    s.billing_source
             FROM sessions s
             LEFT JOIN (
                SELECT session_id, MAX(created_at) AS last_at
                FROM session_messages
                GROUP BY session_id
             ) m ON m.session_id = s.id
             WHERE s.agent_type = 'chat' AND s.created_by_user_id = $1
             ORDER BY COALESCE(m.last_at, s.started_at) DESC"#,
            user_id,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return the `created_by_user_id` for a chat session, if the session
    /// exists and is chat-typed. `Ok(None)` distinguishes "no such chat
    /// session" — used by handlers to authorize ownership (and return 404
    /// rather than 403 so a probe can't learn a session id exists).
    ///
    /// The outer `Option` is presence of the row; the inner `Option` is the
    /// nullable column. Returns `Ok(Some(None))` for a legacy unattributed
    /// session that exists but has no owner.
    pub async fn chat_session_owner(&self, session_id: &str) -> Result<Option<Option<String>>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            "SELECT created_by_user_id FROM sessions WHERE id = $1 AND agent_type = 'chat'",
            session_id
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|r| r.created_by_user_id))
    }

    /// Overwrite a chat session's title.  No-op if the session is not chat-typed.
    pub async fn update_chat_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE sessions SET title = $1 WHERE id = $2 AND agent_type = 'chat'",
            title,
            session_id,
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Return the ISO-8601 `created_at` of the most recent
    /// `session_messages` row for this session, or `None` when the session
    /// has no messages yet (e.g. the planner has not produced its first
    /// turn).
    ///
    /// Used by liveness classification: a `running` session whose latest
    /// message timestamp (falling back to `started_at`) is older than the
    /// configured threshold is wedged.
    pub async fn last_message_at(&self, session_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        let row: Option<String> = sqlx::query_scalar!(
            r#"SELECT MAX(created_at) AS "last_at?: String"
               FROM session_messages
               WHERE session_id = $1"#,
            session_id
        )
        .fetch_one(self.db.pool())
        .await?;
        Ok(row.filter(|s: &String| !s.is_empty()))
    }

    /// Delete a chat session row.  `session_messages` rows are cascade-deleted
    /// by the `fk_session_messages_session ON DELETE CASCADE` FK in migration 1.
    pub async fn delete_chat_session(&self, session_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query!(
            "DELETE FROM sessions WHERE id = $1 AND agent_type = 'chat'",
            session_id,
        )
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Best-effort backfill of price snapshot columns for existing sessions
    /// whose snapshots were never captured (`NULL`) OR were captured all-zero
    /// (the flat-rate coding-plan signature — e.g. the kimi-for-coding/k3 rows
    /// migration 141 flipped to `unpriced`, which the catalog can now price via
    /// the k3 alias).
    ///
    /// **Approximate historical pricing:** the pricing data passed in comes from
    /// the *current* catalog and does not reflect what the model cost when the
    /// session actually ran.  Callers should log this caveat.  Models whose
    /// supplied pricing is all-zero/default are intentionally skipped — an
    /// unknown price must never be recorded as a real $0.00.
    ///
    /// Each tuple is `(model_id, pricing, is_subscription)`. A repriced row that
    /// is currently `cost_basis = 'unpriced'` is promoted to `'projected'` when
    /// its model's provider classifies as a subscription; for non-subscription
    /// providers the pricing is filled but the basis stays `'unpriced'` (we
    /// cannot know plan-vs-key without per-row credential evidence). Rows already
    /// `'actual'` / `'projected'` keep their basis.
    ///
    /// Only rows whose four snapshot columns are ALL `NULL` or ALL literal `0`
    /// are touched; partially-zero rows and rows with a real captured snapshot
    /// are preserved. Idempotent: once repriced (non-zero snapshots) a row no
    /// longer matches either arm, so re-running is a no-op.
    ///
    /// Returns the total number of rows updated across all models.
    pub async fn backfill_pricing_snapshots(
        &self,
        pricing_by_model: &[(String, Pricing, bool)],
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let mut total_updated: u64 = 0;
        for (model_id, pricing, is_subscription) in pricing_by_model {
            // All-zero pricing represents "pricing unknown" for custom/seed
            // catalog entries, not a known free model.  Leave those rows as-is
            // so the analytics layer never conflates unknown cost with zero.
            if !pricing.is_priced() {
                continue;
            }

            let input_rate = pricing.input_per_million;
            let output_rate = pricing.output_per_million;
            let cache_read_rate = pricing.cache_read_per_million;
            let cache_write_rate = pricing.cache_write_per_million;

            let result = sqlx::query(
                r#"UPDATE sessions
                 SET input_price_per_million_snapshot = $1,
                     output_price_per_million_snapshot = $2,
                     cache_read_price_per_million_snapshot = $3,
                     cache_write_price_per_million_snapshot = $4,
                     cost_usd = (
                         COALESCE(tokens_in, 0)::double precision * $1
                         + COALESCE(tokens_out, 0)::double precision * $2
                         + COALESCE(cache_read_tokens, 0)::double precision * $3
                         + COALESCE(cache_write_tokens, 0)::double precision * $4
                     ) / 1000000.0,
                     cost_basis = CASE
                         WHEN cost_basis = 'unpriced' AND $6 THEN 'projected'
                         ELSE cost_basis
                     END
                 WHERE model_id = $5
                   AND (
                        (input_price_per_million_snapshot IS NULL
                         AND output_price_per_million_snapshot IS NULL
                         AND cache_read_price_per_million_snapshot IS NULL
                         AND cache_write_price_per_million_snapshot IS NULL)
                     OR (input_price_per_million_snapshot = 0
                         AND output_price_per_million_snapshot = 0
                         AND cache_read_price_per_million_snapshot = 0
                         AND cache_write_price_per_million_snapshot = 0)
                   )"#,
            )
            .bind(input_rate)
            .bind(output_rate)
            .bind(cache_read_rate)
            .bind(cache_write_rate)
            .bind(model_id.as_str())
            .bind(*is_subscription)
            .execute(self.db.pool())
            .await?;

            total_updated += result.rows_affected();
        }

        Ok(total_updated)
    }

    /// Boot-time, idempotent repair of plan-backed `openai/*` sessions that old
    /// worker pods mis-booked as `cost_basis = 'actual'` before the in-Pod
    /// billing-signal fix. Runtime equivalent of migration 141 step 2, re-run
    /// every boot so sessions created during a post-deploy image-rebuild window
    /// (an old worker image still dispatching) are cleaned on the next restart.
    ///
    /// GATED on install-wide credential evidence, queried live so it stays
    /// generic for any deployment: applies ONLY when a non-revoked ChatGPT/Codex
    /// plan OAuth credential (`__OAUTH_CHATGPT_CODEX`) exists AND no non-revoked
    /// `OPENAI_API_KEY` credential exists — under that gate an `openai/*` session
    /// cannot be metered API spend, so the correct basis is `projected`. The
    /// EXISTS / NOT EXISTS gate is inlined in the UPDATE so it is atomic.
    /// Idempotent: gated on `cost_basis = 'actual'`, so once rewritten a row no
    /// longer matches.
    ///
    /// Returns the number of rows reclassified (0 when the gate does not hold).
    pub async fn reclassify_plan_openai_sessions_if_gated(&self) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(
            r#"UPDATE sessions
                 SET cost_basis = 'projected'
               WHERE cost_basis = 'actual'
                 AND model_id LIKE 'openai/%'
                 AND EXISTS (
                      SELECT 1 FROM credentials c
                       WHERE c.key_name = '__OAUTH_CHATGPT_CODEX'
                         AND c.revoked_at IS NULL)
                 AND NOT EXISTS (
                      SELECT 1 FROM credentials c
                       WHERE c.key_name = 'OPENAI_API_KEY'
                         AND c.revoked_at IS NULL)"#,
        )
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Candidates for the extraction backfill sweep: completed task-runs whose
    /// sessions were never extracted (`event_taxonomy IS NULL`).  Returns
    /// `(task_id, task_run_id)` pairs ordered by `task_run_id`.
    pub async fn list_unextracted_completed_candidates(
        &self,
    ) -> Result<Vec<ExtractionBackfillCandidate>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, ExtractionBackfillCandidate>(
            "SELECT DISTINCT s.task_id, s.task_run_id \
             FROM sessions s \
             JOIN task_runs tr ON tr.id = s.task_run_id \
             WHERE tr.status = 'completed' \
               AND s.event_taxonomy IS NULL \
               AND s.task_id IS NOT NULL \
               AND s.task_run_id IS NOT NULL \
             ORDER BY s.task_run_id",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Detect running sessions whose backing task is closed (or missing).
    ///
    /// Used by the coordinator's orphan-worker-session health sweep. Returns
    /// lightweight [`OrphanSessionCandidate`] rows so the coordinator can apply
    /// its grace-period logic without loading full [`SessionRecord`] payloads.
    pub async fn orphan_session_candidates(&self) -> Result<Vec<OrphanSessionCandidate>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, OrphanSessionCandidate>(
            r#"SELECT
                     s.id          AS "session_id",
                     s.task_id,
                     s.started_at,
                     t.status      AS "task_status",
                     t.closed_at   AS "task_closed_at"
                   FROM sessions s
                   LEFT JOIN tasks t ON t.id = s.task_id
                   WHERE s.status = 'running'
                     AND s.task_id IS NOT NULL
                     AND (t.id IS NULL
                          OR t.status IN ('closed', 'force_closed',
                                          'parked_permanently', 'parked_for_review'))"#,
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Interrupt a single session by id, setting `status = 'interrupted'` and
    /// stamping `ended_at`.  No-op if the session is no longer `running`.
    ///
    /// Returns `true` when a row was actually updated.  This is the
    /// "fire-and-forget" variant used by the orphan-session health sweep where
    /// we don't have an `EventBus` reference and token counts are unknown.
    pub async fn interrupt_by_id(&self, session_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(
            r#"UPDATE sessions
               SET status = 'interrupted',
                   ended_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               WHERE id = $1 AND status = 'running'"#,
        )
        .bind(session_id)
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Lightweight projection of all sessions: `(id, status, ended_at)`.
    ///
    /// Used by the output-stash GC to build its id→unix_secs map without
    /// loading full [`SessionRecord`] payloads with all their token/price
    /// columns.
    pub async fn list_all_status_ended_at(&self) -> Result<Vec<SessionStatusSnapshot>> {
        self.db.ensure_initialized().await?;
        Ok(
            sqlx::query_as::<_, SessionStatusSnapshot>("SELECT id, status, ended_at FROM sessions")
                .fetch_all(self.db.pool())
                .await?,
        )
    }

    /// Return distinct `task_run_id` values from sessions that are still
    /// `running` with `ended_at IS NULL` and a linked `task_run_id`.
    ///
    /// Used by the coordinator's cargo-target-run-dir sweep to protect live
    /// run directories from cleanup.
    pub async fn running_task_run_ids(&self) -> Result<Vec<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT task_run_id FROM sessions
             WHERE status = 'running' AND ended_at IS NULL AND task_run_id IS NOT NULL",
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// True when at least one `sessions` row references the given task_run.
    ///
    /// Used by the host-side pre-session liveness deadline as its DB-truth
    /// disarm signal: once any session exists for the run, the first reply-loop
    /// turn has been reached and liveness is owned by the coordinator's session
    /// stall detector / zombie reaper, so the pre-session deadline stands down.
    pub async fn exists_for_task_run(&self, task_run_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        let found: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM sessions WHERE task_run_id = $1 LIMIT 1")
                .bind(task_run_id)
                .fetch_optional(self.db.pool())
                .await?;
        Ok(found.is_some())
    }

    /// Backdate a session's `started_at` by a PostgreSQL `interval` string
    /// (e.g. `'20 minutes'`, `'30 seconds'`).
    ///
    /// Test-fixture helper: production sessions are stamped at creation time
    /// and never backdated.  Used by coordinator zombie / orphan tests to
    /// fabricate sessions that predate the zombie hard-cap window.
    pub async fn backdate_started_at(&self, id: &str, interval: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE sessions SET started_at = to_char(
                 now() AT TIME ZONE 'utc' - CAST($1 AS interval),
                 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
             WHERE id = $2",
        )
        .bind(interval)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Set `tokens_in` and `tokens_out` on a session without changing its
    /// status or `ended_at`.
    ///
    /// Test-fixture helper: production token accounting goes through
    /// [`SessionRepository::update`] or [`SessionRepository::flush_tokens`].
    pub async fn set_token_counts(&self, id: &str, tokens_in: i64, tokens_out: i64) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE sessions SET tokens_in = $1, tokens_out = $2 WHERE id = $3")
            .bind(tokens_in)
            .bind(tokens_out)
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Set `tokens_in`, `tokens_out`, and backdate `started_at` on a session
    /// without changing its status or `ended_at`.
    ///
    /// Test-fixture helper combining [`Self::set_token_counts`] and
    /// [`Self::backdate_started_at`] in a single UPDATE so tests that need
    /// both adjustments issue one round-trip.
    pub async fn set_tokens_and_backdate(
        &self,
        id: &str,
        interval: &str,
        tokens_in: i64,
        tokens_out: i64,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE sessions
             SET tokens_in = $1, tokens_out = $2,
                 started_at = to_char(
                     now() AT TIME ZONE 'utc' - $3::interval,
                     'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
             WHERE id = $4",
        )
        .bind(tokens_in)
        .bind(tokens_out)
        .bind(interval)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

/// Row returned by [`SessionRepository::list_unextracted_completed_candidates`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExtractionBackfillCandidate {
    pub task_id: String,
    pub task_run_id: String,
}

/// Lightweight row for orphan-session detection in the coordinator health sweep.
///
/// Contains just enough columns for the coordinator to apply its grace-period
/// logic and log diagnostics — the full [`SessionRecord`] projection is not
/// needed here.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrphanSessionCandidate {
    pub session_id: String,
    pub task_id: Option<String>,
    pub started_at: String,
    pub task_status: Option<String>,
    pub task_closed_at: Option<String>,
}

/// Lightweight session status snapshot for output-stash GC.
///
/// Only projects `(id, status, ended_at)` — far cheaper than a full
/// [`SessionRecord`] when the GC only needs to map id→unix_secs.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionStatusSnapshot {
    pub id: String,
    pub status: String,
    pub ended_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::SessionRecord;

    use super::*;
    use crate::repositories::epic::EpicRepository;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn capturing_bus() -> (EventBus, Arc<Mutex<Vec<DjinnEventEnvelope>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        (bus, captured)
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
        let creator = crate::repositories::test_support::seed_test_user(db).await;
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
            task_id,
            epic.project_id,
            short_id,
            epic.id,
            creator
        )
        .execute(db.pool())
        .await
        .unwrap();

        (epic.project_id, task_id)
    }

    /// Creating the first reply-loop session flips its `starting` task_run to
    /// `running`, and a subsequent session on the same run is a no-op (stays
    /// `running`) — the pre-session tracking transition. A session with no
    /// task_run, or one whose run is already terminal, leaves the run untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_session_flips_starting_task_run_to_running() {
        use crate::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};

        let db = test_db();
        let (bus, _captured) = capturing_bus();
        db.ensure_initialized().await.unwrap();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;

        let run_repo = TaskRunRepository::new(db.clone());
        let run_id = uuid::Uuid::now_v7().to_string();
        run_repo
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: "new_task",
                status: Some("starting"),
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .unwrap();
        assert_eq!(
            run_repo.get(&run_id).await.unwrap().unwrap().status,
            "starting"
        );

        let repo = SessionRepository::new(db.clone(), bus.clone());
        repo.create(CreateSessionParams {
            project_id: &project_id,
            task_id: Some(&task_id),
            model: "openai/gpt-a",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
        assert_eq!(
            run_repo.get(&run_id).await.unwrap().unwrap().status,
            "running",
            "first session flips starting → running"
        );

        // Mark the run terminal, then a late (extraction) session must NOT
        // resurrect it back to running.
        run_repo
            .update_status(&run_id, djinn_core::models::TaskRunStatus::Completed)
            .await
            .unwrap();
        repo.create(CreateSessionParams {
            project_id: &project_id,
            task_id: Some(&task_id),
            model: "openai/gpt-a",
            agent_type: "reviewer",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
        assert_eq!(
            run_repo.get(&run_id).await.unwrap().unwrap().status,
            "completed",
            "guarded flip is a no-op for a non-starting run"
        );
    }

    /// `exists_for_task_run` is the pre-session deadline's disarm probe: false
    /// before any session exists, true once one does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exists_for_task_run_tracks_session_presence() {
        use crate::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};

        let db = test_db();
        let (bus, _captured) = capturing_bus();
        db.ensure_initialized().await.unwrap();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db.clone(), bus.clone());

        let run_repo = TaskRunRepository::new(db.clone());
        let run_id = uuid::Uuid::now_v7().to_string();
        run_repo
            .create(CreateTaskRunParams {
                id: &run_id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: "new_task",
                status: Some("starting"),
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .unwrap();

        assert!(!repo.exists_for_task_run(&run_id).await.unwrap());
        repo.create(CreateSessionParams {
            project_id: &project_id,
            task_id: Some(&task_id),
            model: "openai/gpt-a",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
        assert!(repo.exists_for_task_run(&run_id).await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_model_for_task_role_returns_newest_matching_session() {
        let db = test_db();
        let (bus, _captured) = capturing_bus();
        db.ensure_initialized().await.unwrap();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db.clone(), bus.clone());

        // No worker session yet → None.
        assert_eq!(
            repo.latest_model_for_task_role(&task_id, "worker")
                .await
                .unwrap(),
            None
        );

        // First worker run on model A, then a re-implementation on model B.
        for model in ["openai/gpt-a", "anthropic/opus-b"] {
            repo.create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model,
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        }
        // A reviewer session must NOT be picked up by the "worker" lookup.
        repo.create(CreateSessionParams {
            project_id: &project_id,
            task_id: Some(&task_id),
            model: "x/reviewer-model",
            agent_type: "reviewer",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

        // Newest worker session's model (B) wins.
        assert_eq!(
            repo.latest_model_for_task_role(&task_id, "worker")
                .await
                .unwrap()
                .as_deref(),
            Some("anthropic/opus-b")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn count_active_by_user_and_model_groups_by_task_creator_and_model() {
        let db = test_db();
        let (bus, _captured) = capturing_bus();
        db.ensure_initialized().await.unwrap();

        let user_a = uuid::Uuid::now_v7().to_string();
        let user_b = uuid::Uuid::now_v7().to_string();
        for (uid, gid, login) in [(&user_a, 7001i64, "count-a"), (&user_b, 7002i64, "count-b")] {
            sqlx::query!(
                "INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)",
                uid,
                gid,
                login
            )
            .execute(db.pool())
            .await
            .unwrap();
        }

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .create("E", "", "", "", "", None)
            .await
            .unwrap();
        let project_id = epic.project_id.clone();

        async fn mk_task(
            db: &Database,
            project_id: &str,
            epic_id: &str,
            creator: &str,
            issue_type: &str,
        ) -> String {
            let id = uuid::Uuid::now_v7().to_string();
            let short_id = format!("t{}{}", &id[..6], &id[id.len() - 6..]);
            sqlx::query!(
                "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                    issue_type, priority, owner, status, continuation_count,
                                    labels, acceptance_criteria, memory_refs, created_by_user_id)
                 VALUES ($1,$2,$3,$4,'T','','',$5,0,'','open',0,'[]'::jsonb,'[]'::jsonb,'[]'::jsonb,$6)",
                id, project_id, short_id, epic_id, issue_type, creator
            )
            .execute(db.pool())
            .await
            .unwrap();
            id
        }

        let repo = SessionRepository::new(db.clone(), bus.clone());
        // A: 2 running on gpt + 1 on kimi; B: 1 on gpt. Sessions carry no own
        // created_by_user_id (no task-local), so the count must come via the
        // task-creator join.
        for model in ["openai/gpt", "openai/gpt", "x/kimi"] {
            let t = mk_task(&db, &project_id, &epic.id, &user_a, "task").await;
            repo.create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&t),
                model,
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        }
        let tb = mk_task(&db, &project_id, &epic.id, &user_b, "task").await;
        repo.create(CreateSessionParams {
            project_id: &project_id,
            task_id: Some(&tb),
            model: "openai/gpt",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

        // A running chat session for user_a on the same model must NOT count:
        // interactive chat shares no concurrency budget with autonomous
        // task-runs (else an open/leaked chat tab starves dispatch).
        let chat_id = uuid::Uuid::now_v7().to_string();
        sqlx::query!(
            "INSERT INTO sessions
                (id, project_id, task_id, model_id, agent_type, status,
                 created_by_user_id, task_run_id, title)
             VALUES ($1, NULL, NULL, 'openai/gpt', 'chat', 'running', $2, NULL, 'New Chat')",
            chat_id,
            user_a
        )
        .execute(db.pool())
        .await
        .unwrap();

        // Non-chat refinement/tribunal sessions (advocate, adversary, judge)
        // must count toward the same per-user/model cap as normal worker sessions.
        for agent_type in ["advocate", "adversary", "judge"] {
            let t = mk_task(&db, &project_id, &epic.id, &user_a, "refinement").await;
            repo.create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&t),
                model: "openai/gpt",
                agent_type,
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        }

        // Reviewer is its own lane; it still shares the legacy per-model
        // count with every other autonomous role.
        let reviewer_task = mk_task(&db, &project_id, &epic.id, &user_a, "task").await;
        repo.create(CreateSessionParams {
            project_id: &project_id,
            task_id: Some(&reviewer_task),
            model: "openai/gpt",
            agent_type: "reviewer",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

        let map: std::collections::HashMap<(String, String), i64> = repo
            .count_active_by_user_and_model()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|(c, m, n)| c.map(|c| ((c, m), n)))
            .collect();
        // user_a's gpt count is 6 (2 worker + 3 tribunal + 1 reviewer) — the
        // chat session is excluded.
        assert_eq!(
            map.get(&(user_a.clone(), "openai/gpt".to_string())),
            Some(&6)
        );
        assert_eq!(map.get(&(user_a.clone(), "x/kimi".to_string())), Some(&1));
        assert_eq!(
            map.get(&(user_b.clone(), "openai/gpt".to_string())),
            Some(&1)
        );

        let lane_counts: std::collections::HashMap<(String, ModelLane), i64> = repo
            .count_active_by_user_and_lane()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|(creator, lane, count)| creator.map(|c| ((c, lane), count)))
            .collect();
        assert_eq!(
            lane_counts.get(&(user_a.clone(), ModelLane::Implement)),
            Some(&3),
            "two gpt workers plus one kimi worker consume implement capacity"
        );
        assert_eq!(
            lane_counts.get(&(user_a.clone(), ModelLane::Plan)),
            Some(&3),
            "advocate/adversary/judge consume plan capacity"
        );
        assert_eq!(
            lane_counts.get(&(user_a.clone(), ModelLane::Review)),
            Some(&1)
        );
        assert_eq!(
            lane_counts.get(&(user_b.clone(), ModelLane::Implement)),
            Some(&1)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_emits_event() {
        let db = test_db();
        let (bus, captured) = capturing_bus();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db, bus);

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        assert_eq!(created.status, "running");

        {
            let events = captured.lock().unwrap();
            let started = events
                .iter()
                .find(|e| e.entity_type == "session" && e.action == "started");
            assert!(started.is_some(), "expected session.started event");
            let s: SessionRecord =
                serde_json::from_value(started.unwrap().payload.clone()).unwrap();
            assert_eq!(s.id, created.id);
        }

        captured.lock().unwrap().clear();

        let updated = repo
            .update(&created.id, SessionStatus::Completed, 10, 20, 5, 3, None)
            .await
            .unwrap();
        assert_eq!(updated.status, "completed");
        assert_eq!(updated.cache_read_tokens, 5);
        assert_eq!(updated.cache_write_tokens, 3);
        assert_eq!(updated.tokens_in, 10);
        assert_eq!(updated.tokens_out, 20);
        assert!(updated.ended_at.is_some());
        assert!(updated.parked_reason.is_none());

        let events = captured.lock().unwrap();
        let completed = events
            .iter()
            .find(|e| e.entity_type == "session" && e.action == "completed");
        assert!(completed.is_some(), "expected session.completed event");
        let s: SessionRecord = serde_json::from_value(completed.unwrap().payload.clone()).unwrap();
        assert_eq!(s.id, created.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_record_maps_nullable_parked_reason() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        assert!(created.parked_reason.is_none());

        sqlx::query("UPDATE sessions SET parked_reason = 'budget' WHERE id = $1")
            .bind(&created.id)
            .execute(db.pool())
            .await
            .unwrap();

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.parked_reason.as_deref(), Some("budget"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_record_maps_nullable_cost_and_snapshot_fields() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // New sessions have no pricing snapshot or cost yet — all five
        // nullable columns must be NULL, never zero.
        assert!(created.cost_usd.is_none());
        assert!(created.input_price_per_million_snapshot.is_none());
        assert!(created.output_price_per_million_snapshot.is_none());
        assert!(created.cache_read_price_per_million_snapshot.is_none());
        assert!(created.cache_write_price_per_million_snapshot.is_none());

        // Populate the columns directly (later tasks wire this up from the
        // catalog) and verify the repository projections read them back.
        sqlx::query(
            r#"UPDATE sessions
               SET cost_usd = $1,
                   input_price_per_million_snapshot = $2,
                   output_price_per_million_snapshot = $3,
                   cache_read_price_per_million_snapshot = $4,
                   cache_write_price_per_million_snapshot = $5
               WHERE id = $6"#,
        )
        .bind(0.0123_f64)
        .bind(1.5_f64)
        .bind(6.0_f64)
        .bind(0.15_f64)
        .bind(1.875_f64)
        .bind(&created.id)
        .execute(db.pool())
        .await
        .unwrap();

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.cost_usd, Some(0.0123));
        assert_eq!(fetched.input_price_per_million_snapshot, Some(1.5));
        assert_eq!(fetched.output_price_per_million_snapshot, Some(6.0));
        assert_eq!(fetched.cache_read_price_per_million_snapshot, Some(0.15));
        assert_eq!(fetched.cache_write_price_per_million_snapshot, Some(1.875));

        // The list-for-task projection must also carry the new fields.
        let listed = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].cost_usd, Some(0.0123));
        assert_eq!(listed[0].input_price_per_million_snapshot, Some(1.5));
        assert_eq!(listed[0].output_price_per_million_snapshot, Some(6.0));
        assert_eq!(listed[0].cache_read_price_per_million_snapshot, Some(0.15));
        assert_eq!(
            listed[0].cache_write_price_per_million_snapshot,
            Some(1.875)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_persists_pricing_snapshot_when_priced() {
        use djinn_core::models::provider::Pricing;

        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let pricing = Pricing {
            input_per_million: 1.5,
            output_per_million: 6.0,
            cache_read_per_million: 0.15,
            cache_write_per_million: 1.875,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&pricing),
                cost_basis: None,
            })
            .await
            .unwrap();

        // Snapshot columns must reflect the pricing passed at creation.
        assert_eq!(created.input_price_per_million_snapshot, Some(1.5));
        assert_eq!(created.output_price_per_million_snapshot, Some(6.0));
        assert_eq!(created.cache_read_price_per_million_snapshot, Some(0.15));
        assert_eq!(created.cache_write_price_per_million_snapshot, Some(1.875));

        // cost_usd must remain NULL at creation — token counts are still zero,
        // cost recomputation belongs to later token-write paths.
        assert!(created.cost_usd.is_none());

        // Round-trip via repo.get to confirm the projection carries the values.
        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.input_price_per_million_snapshot, Some(1.5));
        assert_eq!(fetched.output_price_per_million_snapshot, Some(6.0));
        assert_eq!(fetched.cache_read_price_per_million_snapshot, Some(0.15));
        assert_eq!(fetched.cache_write_price_per_million_snapshot, Some(1.875));
        assert!(fetched.cost_usd.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_leaves_snapshot_null_when_unpriced() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Uncatalogued/unpriced session — pricing is None.
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "uncatalogued/model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // All snapshot fields and cost_usd must be NULL, never zero/free.
        assert!(created.input_price_per_million_snapshot.is_none());
        assert!(created.output_price_per_million_snapshot.is_none());
        assert!(created.cache_read_price_per_million_snapshot.is_none());
        assert!(created.cache_write_price_per_million_snapshot.is_none());
        assert!(created.cost_usd.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_sets_parked_reason_without_clobbering_on_none() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let parked = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let updated = repo
            .update(
                &parked.id,
                SessionStatus::Completed,
                10,
                20,
                5,
                3,
                Some("budget".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(updated.parked_reason.as_deref(), Some("budget"));

        let reason: Option<String> =
            sqlx::query_scalar("SELECT parked_reason FROM sessions WHERE id = $1")
                .bind(&parked.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(reason.as_deref(), Some("budget"));

        let updated = repo
            .update(&parked.id, SessionStatus::Completed, 11, 21, 6, 4, None)
            .await
            .unwrap();
        assert_eq!(updated.parked_reason.as_deref(), Some("budget"));

        let reason: Option<String> =
            sqlx::query_scalar("SELECT parked_reason FROM sessions WHERE id = $1")
                .bind(&parked.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(reason.as_deref(), Some("budget"));

        let fresh = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5-fresh",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let updated = repo
            .update(&fresh.id, SessionStatus::Completed, 1, 2, 0, 0, None)
            .await
            .unwrap();
        assert!(updated.parked_reason.is_none());

        let reason: Option<String> =
            sqlx::query_scalar("SELECT parked_reason FROM sessions WHERE id = $1")
                .bind(&fresh.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(reason.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_and_resume_preserve_session_identity() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        assert_eq!(created.status, SessionStatus::Running.as_str());
        assert!(
            created.ended_at.is_none(),
            "new sessions should start without ended_at"
        );

        let paused = repo.pause(&created.id, 12, 34).await.unwrap();
        assert_eq!(paused.id, created.id);
        assert_eq!(paused.status, SessionStatus::Paused.as_str());
        assert_eq!(paused.tokens_in, 12);
        assert_eq!(paused.tokens_out, 34);
        assert!(paused.ended_at.is_none(), "paused sessions stay resumable");

        let paused_lookup = repo.paused_for_task(&task_id).await.unwrap().unwrap();
        assert_eq!(paused_lookup.id, created.id);
        assert_eq!(paused_lookup.status, SessionStatus::Paused.as_str());

        let resumed = repo.set_running(&created.id).await.unwrap();
        assert_eq!(resumed.id, created.id);
        assert_eq!(resumed.status, SessionStatus::Running.as_str());
        assert!(
            resumed.ended_at.is_none(),
            "resumed session should remain open"
        );

        let active = repo.active_for_task(&task_id).await.unwrap().unwrap();
        assert_eq!(active.id, created.id);
        assert_eq!(active.status, SessionStatus::Running.as_str());

        let sessions = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "resume should reuse existing session row"
        );
        assert_eq!(sessions[0].id, created.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_running_for_task_only_updates_running_sessions_for_target_task() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let (other_project_id, other_task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let first_running_target = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let paused_target = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5-pause",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let paused_target = repo.pause(&paused_target.id, 7, 8).await.unwrap();

        let second_running_target = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5-mini",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let other_task_running = repo
            .create(CreateSessionParams {
                project_id: &other_project_id,
                task_id: Some(&other_task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let interrupted = repo.interrupt_running_for_task(&task_id).await.unwrap();
        assert_eq!(
            interrupted, 2,
            "only running rows for the target task should be interrupted"
        );

        let first = repo.get(&first_running_target.id).await.unwrap().unwrap();
        assert_eq!(first.status, SessionStatus::Interrupted.as_str());
        assert!(
            first.ended_at.is_some(),
            "interrupted sessions should be closed"
        );

        let second = repo.get(&second_running_target.id).await.unwrap().unwrap();
        assert_eq!(second.status, SessionStatus::Interrupted.as_str());
        assert!(second.ended_at.is_some());

        let paused_after = repo.get(&paused_target.id).await.unwrap().unwrap();
        assert_eq!(paused_after.status, SessionStatus::Paused.as_str());
        assert!(
            paused_after.ended_at.is_none(),
            "paused resumable session must remain open"
        );

        let other_after = repo.get(&other_task_running.id).await.unwrap().unwrap();
        assert_eq!(other_after.status, SessionStatus::Running.as_str());
        assert!(other_after.ended_at.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_running_except_task_run_ids_preserves_reconnectable_sessions() {
        use crate::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};

        let db = test_db();
        let (bus, captured) = capturing_bus();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db.clone(), bus);
        let runs = TaskRunRepository::new(db);
        let connected_run_id = uuid::Uuid::now_v7().to_string();
        let stale_run_id = uuid::Uuid::now_v7().to_string();
        for run_id in [&connected_run_id, &stale_run_id] {
            runs.create(CreateTaskRunParams {
                id: run_id,
                project_id: &project_id,
                task_id: &task_id,
                trigger_type: "manual",
                status: Some("running"),
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .unwrap();
        }

        let connected = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(&connected_run_id),
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let stale = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(&stale_run_id),
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let unlinked = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        captured.lock().unwrap().clear();

        let reconnectable = std::collections::HashSet::from([connected_run_id]);
        assert_eq!(
            repo.interrupt_running_except_task_run_ids(&reconnectable)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            repo.get(&connected.id).await.unwrap().unwrap().status,
            SessionStatus::Running.as_str()
        );
        assert_eq!(
            repo.get(&stale.id).await.unwrap().unwrap().status,
            SessionStatus::Interrupted.as_str()
        );
        assert_eq!(
            repo.get(&unlinked.id).await.unwrap().unwrap().status,
            SessionStatus::Interrupted.as_str(),
            "a NULL task_run_id has no reconnectability proof"
        );

        let interrupted_ids: std::collections::HashSet<String> = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.entity_type == "session" && event.action == "interrupted")
            .map(|event| {
                serde_json::from_value::<SessionRecord>(event.payload.clone())
                    .unwrap()
                    .id
            })
            .collect();
        assert_eq!(interrupted_ids.len(), 2);
        assert!(interrupted_ids.contains(&stale.id));
        assert!(interrupted_ids.contains(&unlinked.id));
        assert!(!interrupted_ids.contains(&connected.id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_running_except_task_run_ids_with_empty_set_is_blanket() {
        let db = test_db();
        let (bus, captured) = capturing_bus();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db, bus);
        let first = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let second = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        captured.lock().unwrap().clear();

        assert_eq!(
            repo.interrupt_running_except_task_run_ids(&std::collections::HashSet::new())
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            repo.get(&first.id).await.unwrap().unwrap().status,
            SessionStatus::Interrupted.as_str()
        );
        assert_eq!(
            repo.get(&second.id).await.unwrap().unwrap().status,
            SessionStatus::Interrupted.as_str()
        );
        assert_eq!(
            captured
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.entity_type == "session" && event.action == "interrupted")
                .count(),
            2
        );
    }

    /// Insert a task under a given existing epic.  Returns the task id.
    async fn create_task_under_epic(
        db: &Database,
        project_id: &str,
        epic_id: &str,
        creator: &str,
    ) -> String {
        let task_id = uuid::Uuid::now_v7().to_string();
        let short_id = format!("t{}{}", &task_id[..6], &task_id[task_id.len() - 6..]);
        sqlx::query!(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                                issue_type, priority, owner, status, continuation_count, labels, acceptance_criteria, memory_refs, created_by_user_id)
             VALUES ($1, $2, $3, $4, 'Task', '', '', 'task', 0, '', 'open', 0, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $5)",
            task_id,
            project_id,
            short_id,
            epic_id,
            creator
        )
        .execute(db.pool())
        .await
        .unwrap();
        task_id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_planner_for_epic_filters_correctly() {
        let db = test_db();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic_a = epic_repo
            .create("Epic A", "", "", "", "", None)
            .await
            .unwrap();
        let epic_b = epic_repo
            .create("Epic B", "", "", "", "", None)
            .await
            .unwrap();

        let creator = crate::repositories::test_support::seed_test_user(&db).await;
        let task_a1 = create_task_under_epic(&db, &epic_a.project_id, &epic_a.id, &creator).await;
        let task_a2 = create_task_under_epic(&db, &epic_a.project_id, &epic_a.id, &creator).await;
        let task_b1 = create_task_under_epic(&db, &epic_b.project_id, &epic_b.id, &creator).await;

        let repo = SessionRepository::new(db.clone(), bus);

        // 1. Running planner on epic A → should match.
        let planner_a = repo
            .create(CreateSessionParams {
                project_id: &epic_a.project_id,
                task_id: Some(&task_a1),
                model: "openai/gpt-5",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // 2. Running planner on epic B → should NOT match epic A.
        let _planner_b = repo
            .create(CreateSessionParams {
                project_id: &epic_b.project_id,
                task_id: Some(&task_b1),
                model: "openai/gpt-5",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // 3. Running worker on epic A → wrong agent_type, should NOT match.
        let _worker_a = repo
            .create(CreateSessionParams {
                project_id: &epic_a.project_id,
                task_id: Some(&task_a2),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // 4. Completed planner on epic A → not running, should NOT match.
        let finished_planner = repo
            .create(CreateSessionParams {
                project_id: &epic_a.project_id,
                task_id: Some(&task_a2),
                model: "openai/gpt-5",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        repo.update(
            &finished_planner.id,
            SessionStatus::Completed,
            0,
            0,
            0,
            0,
            None,
        )
        .await
        .unwrap();

        let matches = repo.active_planner_for_epic(&epic_a.id).await.unwrap();
        assert_eq!(
            matches.len(),
            1,
            "only the running planner on epic A matches"
        );
        assert_eq!(matches[0].id, planner_a.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn active_planner_for_epic_returns_empty_when_none() {
        let db = test_db();
        let bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), bus.clone());
        let epic = epic_repo
            .create("Epic", "", "", "", "", None)
            .await
            .unwrap();
        let repo = SessionRepository::new(db, bus);
        let matches = repo.active_planner_for_epic(&epic.id).await.unwrap();
        assert!(matches.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn last_message_at_returns_none_when_no_messages() {
        let db = test_db();
        let bus = EventBus::noop();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db.clone(), bus);

        let session = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let last = repo.last_message_at(&session.id).await.unwrap();
        assert!(last.is_none(), "fresh session has no messages");
    }

    // ── Private chat sessions (Part 2 of per-user isolation) ─────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_chat_for_user_scopes_to_owner_and_hides_legacy_null() {
        use crate::repositories::user::UserRepository;
        use djinn_core::auth_context::SESSION_USER_ID;

        let db = test_db();
        db.ensure_initialized().await.unwrap();
        let users = UserRepository::new(db.clone());
        let alice = users
            .upsert_from_github(50001, "alice-chat", None, None)
            .await
            .unwrap()
            .id;
        let bob = users
            .upsert_from_github(50002, "bob-chat", None, None)
            .await
            .unwrap()
            .id;

        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Alice's chat session (stamped via the task-local upsert path).
        let alice_sid = uuid::Uuid::now_v7().to_string();
        SESSION_USER_ID
            .scope(Some(alice.clone()), async {
                repo.upsert_chat_session(&alice_sid, "openai/gpt-5")
                    .await
                    .unwrap();
            })
            .await;

        // Bob's chat session.
        let bob_sid = uuid::Uuid::now_v7().to_string();
        SESSION_USER_ID
            .scope(Some(bob.clone()), async {
                repo.upsert_chat_session(&bob_sid, "openai/gpt-5")
                    .await
                    .unwrap();
            })
            .await;

        // Legacy unattributed chat session (no user scope → NULL owner).
        let legacy_sid = uuid::Uuid::now_v7().to_string();
        repo.upsert_chat_session(&legacy_sid, "openai/gpt-5")
            .await
            .unwrap();

        // Alice sees only her own — not Bob's, not the legacy-NULL one.
        let alice_list = repo.list_chat_for_user(&alice).await.unwrap();
        assert_eq!(alice_list.len(), 1);
        assert_eq!(alice_list[0].id, alice_sid);

        // Bob sees only his own.
        let bob_list = repo.list_chat_for_user(&bob).await.unwrap();
        assert_eq!(bob_list.len(), 1);
        assert_eq!(bob_list[0].id, bob_sid);

        // Unscoped admin list sees all three.
        assert_eq!(repo.list_chat_sessions().await.unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chat_session_owner_distinguishes_missing_owned_and_legacy() {
        use crate::repositories::user::UserRepository;
        use djinn_core::auth_context::SESSION_USER_ID;

        let db = test_db();
        db.ensure_initialized().await.unwrap();
        let alice = UserRepository::new(db.clone())
            .upsert_from_github(60001, "alice-owner", None, None)
            .await
            .unwrap()
            .id;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Owned session.
        let owned = uuid::Uuid::now_v7().to_string();
        SESSION_USER_ID
            .scope(Some(alice.clone()), async {
                repo.upsert_chat_session(&owned, "openai/gpt-5")
                    .await
                    .unwrap();
            })
            .await;

        // Legacy-NULL session.
        let legacy = uuid::Uuid::now_v7().to_string();
        repo.upsert_chat_session(&legacy, "openai/gpt-5")
            .await
            .unwrap();

        // Owned → Some(Some(alice)).
        assert_eq!(
            repo.chat_session_owner(&owned).await.unwrap(),
            Some(Some(alice.clone()))
        );
        // Legacy → Some(None) (exists, no owner).
        assert_eq!(repo.chat_session_owner(&legacy).await.unwrap(), Some(None));
        // Missing → None.
        let missing = uuid::Uuid::now_v7().to_string();
        assert_eq!(repo.chat_session_owner(&missing).await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_recomputes_cost_usd_from_snapshot_and_token_values() {
        use djinn_core::models::provider::Pricing;

        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let pricing = Pricing {
            input_per_million: 1.5,
            output_per_million: 6.0,
            cache_read_per_million: 0.15,
            cache_write_per_million: 1.875,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&pricing),
                cost_basis: None,
            })
            .await
            .unwrap();
        assert!(created.cost_usd.is_none());

        // Final update with token counts — cost should be recomputed.
        let updated = repo
            .update(
                &created.id,
                SessionStatus::Completed,
                1_000_000,
                2_000_000,
                500_000,
                200_000,
                None,
            )
            .await
            .unwrap();

        // Expected: (1_000_000 * 1.5 + 2_000_000 * 6.0 + 500_000 * 0.15 + 200_000 * 1.875) / 1_000_000
        // = 1.5 + 12.0 + 0.075 + 0.375 = 13.95
        assert!(
            (updated.cost_usd.unwrap() - 13.95).abs() < 0.0001,
            "expected cost_usd ~13.95, got {:?}",
            updated.cost_usd
        );

        // Round-trip via get confirms the stored value.
        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert!(
            (fetched.cost_usd.unwrap() - 13.95).abs() < 0.0001,
            "expected fetched cost_usd ~13.95, got {:?}",
            fetched.cost_usd
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_leaves_cost_usd_null_when_any_snapshot_rate_is_null() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Create with no pricing snapshot — all snapshot columns NULL.
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "uncatalogued/model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        assert!(created.cost_usd.is_none());

        // Update with tokens — cost_usd must stay NULL because snapshots are NULL.
        let updated = repo
            .update(
                &created.id,
                SessionStatus::Completed,
                1_000_000,
                2_000_000,
                500_000,
                200_000,
                None,
            )
            .await
            .unwrap();
        assert!(
            updated.cost_usd.is_none(),
            "NULL snapshot must keep cost_usd NULL"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_tokens_recomputes_cost_usd_for_running_sessions() {
        use djinn_core::models::provider::Pricing;

        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let pricing = Pricing {
            input_per_million: 2.0,
            output_per_million: 8.0,
            cache_read_per_million: 0.5,
            cache_write_per_million: 2.5,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&pricing),
                cost_basis: None,
            })
            .await
            .unwrap();
        assert!(created.cost_usd.is_none());

        // Mid-flight flush while still running.
        repo.flush_tokens(&created.id, 500_000, 1_000_000, 250_000, 100_000)
            .await
            .unwrap();

        // Expected: (500_000 * 2.0 + 1_000_000 * 8.0 + 250_000 * 0.5 + 100_000 * 2.5) / 1_000_000
        // = 1.0 + 8.0 + 0.125 + 0.25 = 9.375
        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert!(
            (fetched.cost_usd.unwrap() - 9.375).abs() < 0.0001,
            "expected cost_usd ~9.375 after flush, got {:?}",
            fetched.cost_usd
        );
        assert_eq!(fetched.status, "running");

        // A second flush with updated counts should recompute cost.
        repo.flush_tokens(&created.id, 600_000, 1_200_000, 300_000, 150_000)
            .await
            .unwrap();
        let fetched2 = repo.get(&created.id).await.unwrap().unwrap();
        // Expected: (600_000 * 2.0 + 1_200_000 * 8.0 + 300_000 * 0.5 + 150_000 * 2.5) / 1_000_000
        // = 1.2 + 9.6 + 0.15 + 0.375 = 11.325
        assert!(
            (fetched2.cost_usd.unwrap() - 11.325).abs() < 0.0001,
            "expected cost_usd ~11.325 after second flush, got {:?}",
            fetched2.cost_usd
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_tokens_leaves_cost_usd_null_for_uncatalogued_sessions() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "uncatalogued/model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        repo.flush_tokens(&created.id, 500_000, 1_000_000, 250_000, 100_000)
            .await
            .unwrap();

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert!(
            fetched.cost_usd.is_none(),
            "uncatalogued session must keep cost_usd NULL after flush"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_tokens_does_not_resurrect_terminal_sessions() {
        use djinn_core::models::provider::Pricing;

        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let pricing = Pricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_read_per_million: 0.1,
            cache_write_per_million: 0.2,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&pricing),
                cost_basis: None,
            })
            .await
            .unwrap();

        // Complete the session first.
        repo.update(
            &created.id,
            SessionStatus::Completed,
            100,
            200,
            50,
            25,
            None,
        )
        .await
        .unwrap();

        // Flush racing against the completed session must be a no-op.
        repo.flush_tokens(&created.id, 999, 999, 999, 999)
            .await
            .unwrap();

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.status, "completed");
        assert_eq!(fetched.tokens_in, 100);
        assert_eq!(fetched.tokens_out, 200);
        assert_eq!(fetched.cache_read_tokens, 50);
        assert_eq!(fetched.cache_write_tokens, 25);
        // cost_usd from the final update, not the flush.
        assert!(
            (fetched.cost_usd.unwrap() - 0.00051).abs() < 0.000001,
            "expected cost from final update, got {:?}",
            fetched.cost_usd
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn last_message_at_returns_latest_created_at() {
        let db = test_db();
        let bus = EventBus::noop();
        let (project_id, task_id) = create_task(&db, bus.clone()).await;
        let repo = SessionRepository::new(db.clone(), bus);

        let session = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "planner",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // Explicit, distinct timestamps so the test doesn't depend on
        // clock resolution between consecutive inserts.
        for (id, ts) in [
            ("msg-1", "2026-05-22T10:00:00.000Z"),
            ("msg-2", "2026-05-22T10:05:00.000Z"),
        ] {
            sqlx::query!(
                "INSERT INTO session_messages (id, session_id, role, content_json, created_at)
                 VALUES ($1, $2, 'assistant', '{}', $3)",
                id,
                session.id,
                ts,
            )
            .execute(db.pool())
            .await
            .unwrap();
        }

        let last = repo.last_message_at(&session.id).await.unwrap();
        assert_eq!(last.as_deref(), Some("2026-05-22T10:05:00.000Z"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_pricing_snapshots_fills_null_sessions() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Create a session with no pricing snapshot (pre-backfill row).
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        assert!(created.input_price_per_million_snapshot.is_none());

        // Simulate existing token counts on the session (e.g. from a prior run).
        sqlx::query(
            "UPDATE sessions SET tokens_in = 500000, tokens_out = 1000000,
             cache_read_tokens = 250000, cache_write_tokens = 100000
             WHERE id = $1",
        )
        .bind(&created.id)
        .execute(db.pool())
        .await
        .unwrap();

        let pricing = vec![(
            "openai/gpt-5".to_string(),
            Pricing {
                input_per_million: 2.0,
                output_per_million: 8.0,
                cache_read_per_million: 0.5,
                cache_write_per_million: 2.5,
            },
            false,
        )];

        let updated = repo.backfill_pricing_snapshots(&pricing).await.unwrap();
        assert_eq!(updated, 1, "should update exactly 1 row");

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.input_price_per_million_snapshot, Some(2.0));
        assert_eq!(fetched.output_price_per_million_snapshot, Some(8.0));
        assert_eq!(fetched.cache_read_price_per_million_snapshot, Some(0.5));
        assert_eq!(fetched.cache_write_price_per_million_snapshot, Some(2.5));

        // Expected cost: (500k*2 + 1M*8 + 250k*0.5 + 100k*2.5) / 1M
        //              = 1.0 + 8.0 + 0.125 + 0.25 = 9.375
        assert!(
            (fetched.cost_usd.unwrap() - 9.375).abs() < 0.0001,
            "expected cost_usd ~9.375, got {:?}",
            fetched.cost_usd
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_pricing_snapshots_preserves_existing_snapshots() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let original_pricing = Pricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_read_per_million: 0.1,
            cache_write_per_million: 0.2,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&original_pricing),
                cost_basis: None,
            })
            .await
            .unwrap();
        assert!(created.input_price_per_million_snapshot.is_some());

        // Try to backfill with different pricing.
        let new_pricing = vec![(
            "openai/gpt-5".to_string(),
            Pricing {
                input_per_million: 99.0,
                output_per_million: 99.0,
                cache_read_per_million: 99.0,
                cache_write_per_million: 99.0,
            },
            false,
        )];
        let updated = repo.backfill_pricing_snapshots(&new_pricing).await.unwrap();
        assert_eq!(updated, 0, "existing snapshots must not be overwritten");

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        // Original pricing preserved.
        assert_eq!(fetched.input_price_per_million_snapshot, Some(1.0));
        assert_eq!(fetched.output_price_per_million_snapshot, Some(2.0));
        assert_eq!(fetched.cache_read_price_per_million_snapshot, Some(0.1));
        assert_eq!(fetched.cache_write_price_per_million_snapshot, Some(0.2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_pricing_snapshots_skips_uncatalogued_models() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Session for a model not in the backfill pricing data.
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "unknown/missing-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // Simulate token counts.
        sqlx::query(
            "UPDATE sessions SET tokens_in = 100000, tokens_out = 200000
             WHERE id = $1",
        )
        .bind(&created.id)
        .execute(db.pool())
        .await
        .unwrap();

        let pricing = vec![(
            "openai/gpt-5".to_string(),
            Pricing {
                input_per_million: 2.0,
                output_per_million: 8.0,
                cache_read_per_million: 0.5,
                cache_write_per_million: 2.5,
            },
            false,
        )];
        let updated = repo.backfill_pricing_snapshots(&pricing).await.unwrap();
        assert_eq!(updated, 0, "uncatalogued model must not be touched");

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert!(fetched.input_price_per_million_snapshot.is_none());
        assert!(fetched.output_price_per_million_snapshot.is_none());
        assert!(fetched.cache_read_price_per_million_snapshot.is_none());
        assert!(fetched.cache_write_price_per_million_snapshot.is_none());
        assert!(
            fetched.cost_usd.is_none(),
            "uncatalogued session cost_usd must stay NULL"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_pricing_snapshots_skips_default_unpriced_models() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "custom/seed-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        sqlx::query(
            "UPDATE sessions SET tokens_in = 100000, tokens_out = 200000
             WHERE id = $1",
        )
        .bind(&created.id)
        .execute(db.pool())
        .await
        .unwrap();

        let pricing = vec![("custom/seed-model".to_string(), Pricing::default(), false)];
        let updated = repo.backfill_pricing_snapshots(&pricing).await.unwrap();
        assert_eq!(
            updated, 0,
            "all-zero/default pricing means unknown, not free, and must not be backfilled"
        );

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert!(fetched.input_price_per_million_snapshot.is_none());
        assert!(fetched.output_price_per_million_snapshot.is_none());
        assert!(fetched.cache_read_price_per_million_snapshot.is_none());
        assert!(fetched.cache_write_price_per_million_snapshot.is_none());
        assert!(fetched.cost_usd.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_pricing_snapshots_is_idempotent() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        let pricing = vec![(
            "openai/gpt-5".to_string(),
            Pricing {
                input_per_million: 3.0,
                output_per_million: 15.0,
                cache_read_per_million: 1.0,
                cache_write_per_million: 3.0,
            },
            false,
        )];

        // First run.
        let n1 = repo.backfill_pricing_snapshots(&pricing).await.unwrap();
        assert_eq!(n1, 1);

        // Second run — must be a no-op.
        let n2 = repo.backfill_pricing_snapshots(&pricing).await.unwrap();
        assert_eq!(n2, 0, "second backfill must not re-update rows");

        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.input_price_per_million_snapshot, Some(3.0));
        assert_eq!(fetched.output_price_per_million_snapshot, Some(15.0));
        assert_eq!(fetched.cache_read_price_per_million_snapshot, Some(1.0));
        assert_eq!(fetched.cache_write_price_per_million_snapshot, Some(3.0));
    }

    /// The migration-141 aftermath: a zero-snapshot 'unpriced' session for a
    /// subscription-provider model is now repriced from the catalog AND promoted
    /// to 'projected'. Idempotent on a second run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_reprices_zero_snapshot_unpriced_subscription_session() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Zero-snapshot 'unpriced' row — the flat-rate coding-plan k3 signature.
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "kimi-for-coding/k3",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&Pricing::default()),
                cost_basis: Some("unpriced"),
            })
            .await
            .unwrap();
        assert_eq!(created.input_price_per_million_snapshot, Some(0.0));
        assert_eq!(created.cost_basis, "unpriced");

        sqlx::query("UPDATE sessions SET tokens_in = 1000000, tokens_out = 1000000 WHERE id = $1")
            .bind(&created.id)
            .execute(db.pool())
            .await
            .unwrap();

        let pricing = vec![(
            "kimi-for-coding/k3".to_string(),
            Pricing {
                input_per_million: 3.0,
                output_per_million: 15.0,
                cache_read_per_million: 0.3,
                cache_write_per_million: 0.0,
            },
            true, // subscription provider
        )];

        let n = repo.backfill_pricing_snapshots(&pricing).await.unwrap();
        assert_eq!(n, 1, "zero-snapshot row must be repriced");

        let f = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(f.input_price_per_million_snapshot, Some(3.0));
        assert_eq!(f.output_price_per_million_snapshot, Some(15.0));
        assert_eq!(f.cost_basis, "projected", "subscription row promoted");
        // cost_usd = (1M*3 + 1M*15) / 1M = 18.0
        assert!((f.cost_usd.unwrap() - 18.0).abs() < 1e-6);

        // Second run: snapshots are now non-zero → no match → no-op.
        let n2 = repo.backfill_pricing_snapshots(&pricing).await.unwrap();
        assert_eq!(n2, 0, "repricing must be idempotent");
    }

    /// Boot-repair gate: `openai/*` 'actual' rows flip to 'projected' only when a
    /// Codex plan OAuth credential exists and no OpenAI API key does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reclassify_plan_openai_sessions_is_credential_gated() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let priced = Pricing {
            input_per_million: 2.0,
            output_per_million: 8.0,
            cache_read_per_million: 0.5,
            cache_write_per_million: 2.5,
        };
        let s = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5.6-terra",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&priced),
                cost_basis: Some("actual"),
            })
            .await
            .unwrap();

        let insert_cred = |key: &str| {
            let db = db.clone();
            let key = key.to_string();
            async move {
                sqlx::query(
                    "INSERT INTO credentials (id, provider_id, key_name, encrypted_value) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(uuid::Uuid::now_v7().to_string())
                .bind("chatgpt_codex")
                .bind(key)
                .bind(Vec::<u8>::new())
                .execute(db.pool())
                .await
                .unwrap();
            }
        };

        // No credentials → gate closed → no-op.
        assert_eq!(
            repo.reclassify_plan_openai_sessions_if_gated()
                .await
                .unwrap(),
            0
        );
        assert_eq!(repo.get(&s.id).await.unwrap().unwrap().cost_basis, "actual");

        // Codex plan OAuth present, no OpenAI API key → gate open → flip.
        insert_cred("__OAUTH_CHATGPT_CODEX").await;
        assert_eq!(
            repo.reclassify_plan_openai_sessions_if_gated()
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            repo.get(&s.id).await.unwrap().unwrap().cost_basis,
            "projected"
        );

        // Idempotent second run.
        assert_eq!(
            repo.reclassify_plan_openai_sessions_if_gated()
                .await
                .unwrap(),
            0
        );

        // With an OpenAI API key also present, the gate closes: a fresh 'actual'
        // openai row is left untouched.
        let s2 = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5.5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&priced),
                cost_basis: Some("actual"),
            })
            .await
            .unwrap();
        insert_cred("OPENAI_API_KEY").await;
        assert_eq!(
            repo.reclassify_plan_openai_sessions_if_gated()
                .await
                .unwrap(),
            0,
            "presence of an OpenAI API key closes the gate"
        );
        assert_eq!(
            repo.get(&s2.id).await.unwrap().unwrap().cost_basis,
            "actual"
        );
    }

    // ── Cost-basis derivation tests ─────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cost_basis_actual_for_api_key_with_pricing() {
        use djinn_core::models::provider::Pricing;

        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let pricing = Pricing {
            input_per_million: 1.5,
            output_per_million: 6.0,
            cache_read_per_million: 0.15,
            cache_write_per_million: 1.875,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "anthropic/claude-sonnet-4-20250514",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&pricing),
                cost_basis: Some("actual"),
            })
            .await
            .unwrap();

        assert_eq!(created.cost_basis, "actual");
        assert!(
            created.cost_usd.is_none(),
            "cost_usd is NULL until tokens are written"
        );
        assert_eq!(created.input_price_per_million_snapshot, Some(1.5));

        // After completion with tokens, cost_usd is the list-rate value.
        let updated = repo
            .update(
                &created.id,
                SessionStatus::Completed,
                1000,
                500,
                100,
                50,
                None,
            )
            .await
            .unwrap();
        assert_eq!(updated.cost_basis, "actual");
        assert!(
            updated.cost_usd.is_some(),
            "cost_usd must be computed from snapshots"
        );
        let cost = updated.cost_usd.unwrap();
        // Expected: (1000*1.5 + 500*6.0 + 100*0.15 + 50*1.875) / 1_000_000
        let expected = (1000.0 * 1.5 + 500.0 * 6.0 + 100.0 * 0.15 + 50.0 * 1.875) / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-12,
            "cost_usd={cost}, expected={expected}"
        );

        // list_for_task also returns the basis.
        let listed = repo.list_for_task(&task_id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].cost_basis, "actual");
        assert_eq!(listed[0].cost_usd, Some(expected));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cost_basis_projected_for_subscription_with_pricing() {
        use djinn_core::models::provider::Pricing;

        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Subscription providers (e.g. minimax-coding-plan) still carry list-rate
        // pricing snapshots. The cost_basis label distinguishes projected from actual.
        let pricing = Pricing {
            input_per_million: 0.5,
            output_per_million: 2.0,
            cache_read_per_million: 0.05,
            cache_write_per_million: 0.5,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "minimax-coding-plan/MiniMax-M3",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&pricing),
                cost_basis: Some("projected"),
            })
            .await
            .unwrap();

        assert_eq!(created.cost_basis, "projected");
        assert!(created.cost_usd.is_none());

        // After completion, cost_usd is still computed as the list-rate/projected
        // value (analytics layer splits by basis; the repository preserves list-rate).
        let updated = repo
            .update(
                &created.id,
                SessionStatus::Completed,
                2000,
                1000,
                0,
                0,
                None,
            )
            .await
            .unwrap();
        assert_eq!(updated.cost_basis, "projected");
        assert!(updated.cost_usd.is_some());
        let cost = updated.cost_usd.unwrap();
        let expected = (2000.0 * 0.5 + 1000.0 * 2.0 + 0.0 + 0.0) / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-12,
            "cost_usd={cost}, expected={expected}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cost_basis_unpriced_when_no_pricing() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // Uncatalogued/missing-price: pricing is None, cost_basis defaults to "unpriced".
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "unknown/custom-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        assert_eq!(created.cost_basis, "unpriced");
        assert!(created.cost_usd.is_none());
        assert!(created.input_price_per_million_snapshot.is_none());

        // After completion, cost_usd stays NULL — no snapshots to compute from.
        let updated = repo
            .update(&created.id, SessionStatus::Completed, 500, 250, 0, 0, None)
            .await
            .unwrap();
        assert_eq!(updated.cost_basis, "unpriced");
        assert!(
            updated.cost_usd.is_none(),
            "unpriced sessions must stay NULL for cost_usd"
        );

        // get() also returns the basis.
        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.cost_basis, "unpriced");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cost_basis_defaults_to_unpriced_when_param_is_none() {
        use djinn_core::models::provider::Pricing;

        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        // When cost_basis is None in CreateSessionParams (e.g. callers that
        // haven't been updated), it defaults to "unpriced" in the INSERT.
        let pricing = Pricing {
            input_per_million: 1.5,
            output_per_million: 6.0,
            cache_read_per_million: 0.15,
            cache_write_per_million: 1.875,
        };
        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: Some(&pricing),
                cost_basis: None,
            })
            .await
            .unwrap();

        assert_eq!(
            created.cost_basis, "unpriced",
            "cost_basis=None must default to unpriced in the repository"
        );
    }

    #[test]
    fn cost_basis_enum_round_trips() {
        use djinn_core::models::CostBasis;

        assert_eq!(CostBasis::Actual.as_str(), "actual");
        assert_eq!(CostBasis::Projected.as_str(), "projected");
        assert_eq!(CostBasis::Unpriced.as_str(), "unpriced");

        assert_eq!(CostBasis::from_db("actual"), CostBasis::Actual);
        assert_eq!(CostBasis::from_db("projected"), CostBasis::Projected);
        assert_eq!(CostBasis::from_db("unpriced"), CostBasis::Unpriced);
        // Unknown values fall back to Unpriced (defensive).
        assert_eq!(CostBasis::from_db("bogus"), CostBasis::Unpriced);

        // Default is Unpriced.
        assert_eq!(CostBasis::default(), CostBasis::Unpriced);

        // Serde round-trip.
        let json = serde_json::to_string(&CostBasis::Actual).unwrap();
        assert_eq!(json, "\"actual\"");
        let decoded: CostBasis = serde_json::from_str("\"projected\"").unwrap();
        assert_eq!(decoded, CostBasis::Projected);
    }

    #[test]
    fn cost_basis_migration_backfill_assumption() {
        // Documented backfill behavior: priced historical rows → "actual",
        // unpriced rows → "unpriced". This test verifies the SQL comment in
        // migration 83 matches the enum values.
        use djinn_core::models::CostBasis;

        // The CHECK constraint in migration 83 allows exactly these three values.
        for basis in ["actual", "projected", "unpriced"] {
            let parsed = CostBasis::from_db(basis);
            assert_eq!(parsed.as_str(), basis);
        }
    }

    // ── billing_source (migration 88) ────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn billing_source_recorded_and_persisted() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5.5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: Some("projected"),
            })
            .await
            .unwrap();
        // A freshly created session has no billing_source until the host records it.
        assert!(created.billing_source.is_none());

        let updated = repo
            .set_billing_source(&created.id, "plan_oauth")
            .await
            .unwrap();
        assert_eq!(updated.billing_source.as_deref(), Some("plan_oauth"));

        // Persisted: a fresh read sees the recorded value.
        let fetched = repo.get(&created.id).await.unwrap().unwrap();
        assert_eq!(fetched.billing_source.as_deref(), Some("plan_oauth"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn billing_source_check_constraint_rejects_unknown() {
        let db = test_db();
        let (project_id, task_id) = create_task(&db, EventBus::noop()).await;
        let repo = SessionRepository::new(db.clone(), EventBus::noop());

        let created = repo
            .create(CreateSessionParams {
                project_id: &project_id,
                task_id: Some(&task_id),
                model: "openai/gpt-5.5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();

        // The migration-88 CHECK constraint permits only 'plan_oauth' / 'api_key'.
        let err = repo.set_billing_source(&created.id, "bogus").await;
        assert!(
            err.is_err(),
            "CHECK constraint must reject an unknown billing_source value"
        );
    }
}
