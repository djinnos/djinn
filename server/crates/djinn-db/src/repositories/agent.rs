// djinn:allow-oversize — agent repository over size-guard threshold; already oversized on main, split when touched substantively.
use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_core::models::Agent;

use crate::database::Database;
use crate::{Error, Result};

pub const VALID_BASE_ROLES: &[&str] = &["worker", "lead", "planner", "architect", "reviewer"];

pub struct AgentCreateInput<'a> {
    pub name: &'a str,
    pub base_role: &'a str,
    pub description: &'a str,
    pub system_prompt_extensions: &'a str,
    pub model_preference: Option<&'a str>,
    pub mcp_servers: Option<&'a str>,
    pub skills: Option<&'a str>,
    pub is_default: bool,
}

pub struct AgentUpdateInput<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub system_prompt_extensions: &'a str,
    pub model_preference: Option<&'a str>,
    pub mcp_servers: &'a str,
    pub skills: &'a str,
    /// Final learned_prompt value to persist. Pass None to clear (set NULL).
    /// MCP layer resolves the "keep existing / set / clear" logic before calling.
    pub learned_prompt: Option<&'a str>,
}

pub struct AgentListQuery {
    pub project_id: String,
    pub base_role: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

pub struct AgentListResult {
    pub agents: Vec<Agent>,
    pub total_count: i64,
}

/// Per-role aggregated effectiveness metrics.
pub struct AgentMetrics {
    /// Fraction of closed tasks that completed successfully (0.0–1.0).
    pub success_rate: f64,
    /// Average reopen_count across closed tasks for this role.
    pub avg_reopens: f64,
    /// Number of closed tasks included in calculations.
    pub completed_task_count: i64,
    /// Average total tokens (in + out) per completed session in the window.
    pub avg_tokens: f64,
    /// Average input tokens per completed session in the window.
    pub avg_tokens_in: f64,
    /// Average output tokens per completed session in the window.
    pub avg_tokens_out: f64,
    /// Average session duration in seconds (completed sessions in the window).
    pub avg_time_seconds: f64,
    /// Aggregated extraction-quality counters across sessions in the window.
    pub extraction_quality: ExtractionQualityMetrics,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractionQualityMetrics {
    pub extracted: i64,
    pub dedup_skipped: i64,
    pub novelty_skipped: i64,
    pub written: i64,
    pub merged: i64,
    pub downgraded: i64,
    pub discarded: i64,
}

/// A pending amendment in `learned_prompt_history` that has not yet been
/// evaluated (action = 'keep', no metrics_after recorded).
pub struct PendingAmendmentEvaluation {
    /// History record ID.
    pub history_id: String,
    /// Agent the amendment applies to.
    pub agent_id: String,
    /// ISO-8601 timestamp when the amendment was first applied.
    pub created_at: String,
    /// The amendment text (same as `proposed_text`).
    pub proposed_text: String,
    /// Metrics snapshot at proposal time (JSON string, may be None).
    pub metrics_before: Option<String>,
}

/// Windowed metrics for a role: task counts and averages over a time range.
pub struct WindowedRoleMetrics {
    /// Number of closed tasks completed in the window.
    pub completed_task_count: i64,
    /// Number of closed tasks that failed in the window.
    pub failed_task_count: i64,
    /// Success rate = completed / (completed + failed).
    pub success_rate: f64,
    /// Average total tokens per completed session in the window.
    pub avg_tokens: f64,
}

/// One row from `learned_prompt_history` for a role.
pub struct LearnedPromptHistoryEntry {
    pub id: String,
    pub proposed_text: String,
    pub action: String,
    pub metrics_before: Option<String>,
    pub metrics_after: Option<String>,
    pub created_at: String,
}

// Standard SELECT projection for Agent queries.  `learned_prompt` is derived
// from active `learned_prompt_history` rows rather than the stale text column.
//
// Inline AGENT_COLUMNS projection for each `query_as!(Agent, ...)` call site.
// `query_as!` requires a string-literal SQL argument; `concat!()` doesn't
// satisfy it (verified during batch 4).  Each caller therefore passes the
// full SELECT body as a single raw string literal.

/// Wrap free-text `system_prompt_extensions` content for storage in the
/// `agents.system_prompt_extensions` JSONB column.
///
/// Pre-cut-over this column was `TEXT NOT NULL DEFAULT ''` and held arbitrary
/// prompt text (the model field is a plain `String`; the slot lifecycle appends
/// it verbatim to the base system prompt). The MySQL→Postgres cut-over typed it
/// as JSONB, so we store the text as a JSON *string* value — which round-trips
/// losslessly when read back with `#>> '{}'`. Empty input becomes an empty JSON
/// string rather than erroring or collapsing to `{}` (which would lose data).
fn system_prompt_extensions_value(text: &str) -> serde_json::Value {
    serde_json::Value::String(text.to_owned())
}

pub struct AgentRepository {
    db: Database,
    events: EventBus,
}

impl AgentRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self { db, events }
    }

    /// Return all roles across all projects, ordered by project_id, base_role, name.
    pub async fn list_all(&self) -> Result<Vec<Agent>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Agent,
            r#"SELECT id AS "id!", project_id AS "project_id!",
                name AS "name!", base_role AS "base_role!",
                description AS "description!",
                CASE WHEN jsonb_typeof(system_prompt_extensions) = 'string'
                     THEN system_prompt_extensions #>> '{}' ELSE '' END AS "system_prompt_extensions!",
                model_preference, verification_command,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
                (SELECT string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at AS "created_at!", updated_at AS "updated_at!"
             FROM agents
             ORDER BY project_id ASC, is_default DESC, base_role ASC, name ASC"#
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    /// Return the full `learned_prompt_history` for a role, newest first.
    pub async fn get_history(&self, role_id: &str) -> Result<Vec<LearnedPromptHistoryEntry>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            r#"SELECT id, proposed_text, action AS "action!", metrics_before::text AS "metrics_before?", metrics_after::text AS "metrics_after?", created_at
                 FROM learned_prompt_history
                 WHERE agent_id = $1
                 ORDER BY created_at DESC"#,
            role_id
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| LearnedPromptHistoryEntry {
                id: row.id,
                proposed_text: row.proposed_text,
                action: row.action,
                metrics_before: row.metrics_before,
                metrics_after: row.metrics_after,
                created_at: row.created_at,
            })
            .collect())
    }

    /// Set a role's `learned_prompt` to NULL, discard all active amendments,
    /// and emit an update event.
    pub async fn clear_learned_prompt(&self, role_id: &str) -> Result<Agent> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            r#"UPDATE agents
             SET learned_prompt = NULL,
                 updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
            role_id
        )
        .execute(self.db.pool())
        .await?;

        // Also discard active history rows so the derived learned_prompt stays empty.
        self.clear_amendments(role_id).await?;

        let role = self
            .get(role_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("agent not found: {role_id}")))?;
        self.events.send(DjinnEventEnvelope::agent_updated(&role));
        Ok(role)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Agent>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Agent,
            r#"SELECT id AS "id!", project_id AS "project_id!",
                name AS "name!", base_role AS "base_role!",
                description AS "description!",
                CASE WHEN jsonb_typeof(system_prompt_extensions) = 'string'
                     THEN system_prompt_extensions #>> '{}' ELSE '' END AS "system_prompt_extensions!",
                model_preference, verification_command,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
                (SELECT string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at AS "created_at!", updated_at AS "updated_at!"
             FROM agents WHERE id = $1"#,
            id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return the default role for a given base_role within a project, or None
    /// if no default is configured.
    pub async fn get_default_for_base_role(
        &self,
        project_id: &str,
        base_role: &str,
    ) -> Result<Option<Agent>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Agent,
            r#"SELECT id AS "id!", project_id AS "project_id!",
                name AS "name!", base_role AS "base_role!",
                description AS "description!",
                CASE WHEN jsonb_typeof(system_prompt_extensions) = 'string'
                     THEN system_prompt_extensions #>> '{}' ELSE '' END AS "system_prompt_extensions!",
                model_preference, verification_command,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
                (SELECT string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at AS "created_at!", updated_at AS "updated_at!"
             FROM agents
             WHERE project_id = $1 AND base_role = $2 AND is_default = TRUE LIMIT 1"#,
            project_id,
            base_role
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return an `Agent` by its exact `name` within a project.
    ///
    /// Used by the slot lifecycle when a task has `agent_type` set to a
    /// specialist name (e.g. "rust-expert") to load that role's config.
    pub async fn get_by_name_for_project(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<Option<Agent>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Agent,
            r#"SELECT id AS "id!", project_id AS "project_id!",
                name AS "name!", base_role AS "base_role!",
                description AS "description!",
                CASE WHEN jsonb_typeof(system_prompt_extensions) = 'string'
                     THEN system_prompt_extensions #>> '{}' ELSE '' END AS "system_prompt_extensions!",
                model_preference, verification_command,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
                (SELECT string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at AS "created_at!", updated_at AS "updated_at!"
             FROM agents WHERE project_id = $1 AND name = $2"#,
            project_id,
            name
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Return all roles for a project without pagination — used for the planner
    /// specialist roster where a complete list is always needed.
    pub async fn all_for_project(&self, project_id: &str) -> Result<Vec<Agent>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Agent,
            r#"SELECT id AS "id!", project_id AS "project_id!",
                name AS "name!", base_role AS "base_role!",
                description AS "description!",
                CASE WHEN jsonb_typeof(system_prompt_extensions) = 'string'
                     THEN system_prompt_extensions #>> '{}' ELSE '' END AS "system_prompt_extensions!",
                model_preference, verification_command,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
                (SELECT string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at AS "created_at!", updated_at AS "updated_at!"
             FROM agents
             WHERE project_id = $1 ORDER BY is_default DESC, base_role ASC, name ASC"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?)
    }

    pub async fn create_for_project(
        &self,
        project_id: &str,
        input: AgentCreateInput<'_>,
    ) -> Result<Agent> {
        self.db.ensure_initialized().await?;
        let id = uuid::Uuid::now_v7().to_string();
        let mcp_servers = input.mcp_servers.unwrap_or("[]");
        let skills = input.skills.unwrap_or("[]");
        let is_default_bool = input.is_default;
        // JSONB columns require Value/Json. `mcp_servers`/`skills` are genuine
        // JSON arrays (parse the caller's blob). `system_prompt_extensions` is
        // free-text prompt content (the model field is a plain `String` and the
        // slot lifecycle appends it verbatim) — store it as a JSON *string*
        // value so the JSONB column round-trips the text losslessly.
        let system_prompt_value = system_prompt_extensions_value(input.system_prompt_extensions);
        let mcp_servers_value: serde_json::Value =
            serde_json::from_str(mcp_servers).unwrap_or_else(|_| serde_json::json!([]));
        let skills_value: serde_json::Value =
            serde_json::from_str(skills).unwrap_or_else(|_| serde_json::json!([]));
        sqlx::query!(
            "INSERT INTO agents (
                id, project_id, name, base_role, description,
                system_prompt_extensions, model_preference,
                mcp_servers, skills, is_default
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            id,
            project_id,
            input.name,
            input.base_role,
            input.description,
            system_prompt_value,
            input.model_preference,
            mcp_servers_value,
            skills_value,
            is_default_bool
        )
        .execute(self.db.pool())
        .await?;

        let role = self
            .get(&id)
            .await?
            .ok_or_else(|| Error::InvalidData("agent insert failed".into()))?;
        self.events.send(DjinnEventEnvelope::agent_created(&role));
        Ok(role)
    }

    pub async fn update(&self, id: &str, input: AgentUpdateInput<'_>) -> Result<Agent> {
        self.db.ensure_initialized().await?;
        let system_prompt_value = system_prompt_extensions_value(input.system_prompt_extensions);
        let mcp_servers_value: serde_json::Value = serde_json::from_str(input.mcp_servers)
            .map_err(|e| Error::InvalidData(format!("invalid json for agents.mcp_servers: {e}")))?;
        let skills_value: serde_json::Value = serde_json::from_str(input.skills)
            .map_err(|e| Error::InvalidData(format!("invalid json for agents.skills: {e}")))?;
        sqlx::query!(
            r#"UPDATE agents
             SET name = $1, description = $2, system_prompt_extensions = $3,
                 model_preference = $4,
                 mcp_servers = $5, skills = $6, learned_prompt = $7,
                 updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $8"#,
            input.name,
            input.description,
            system_prompt_value,
            input.model_preference,
            mcp_servers_value,
            skills_value,
            input.learned_prompt,
            id
        )
        .execute(self.db.pool())
        .await?;

        let role = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("agent not found: {id}")))?;
        self.events.send(DjinnEventEnvelope::agent_updated(&role));
        Ok(role)
    }

    /// Set a role as the default for its base_role within a project.
    /// Atomically clears any existing default for the same (project_id, base_role) pair
    /// before marking this role as the new default, satisfying the unique partial index.
    pub async fn set_default(&self, id: &str) -> Result<Agent> {
        self.db.ensure_initialized().await?;

        // Fetch the role so we know its project_id and base_role.
        let role = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("agent not found: {id}")))?;

        // Clear any existing default for this (project_id, base_role).
        sqlx::query!(
            "UPDATE agents SET is_default = FALSE
             WHERE project_id = $1 AND base_role = $2 AND is_default = TRUE",
            role.project_id,
            role.base_role
        )
        .execute(self.db.pool())
        .await?;

        // Set this role as default.
        sqlx::query!(
            r#"UPDATE agents SET is_default = TRUE,
                     updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $1"#,
            id
        )
        .execute(self.db.pool())
        .await?;

        let updated = self.get(id).await?.ok_or_else(|| {
            Error::InvalidData(format!("agent not found after set_default: {id}"))
        })?;
        self.events
            .send(DjinnEventEnvelope::agent_updated(&updated));
        Ok(updated)
    }

    pub async fn delete(&self, id: &str, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!("DELETE FROM agents WHERE id = $1", id)
            .execute(self.db.pool())
            .await?;
        self.events
            .send(DjinnEventEnvelope::agent_deleted(id, project_id));
        Ok(())
    }

    /// Delete every agent row matching `(project_id, base_role)`.
    ///
    /// Used by tests that clear the auto-seeded default rows so they can
    /// install a bespoke one in its place. Emits no events — the test is
    /// about to create a replacement.
    pub async fn delete_for_base_role(&self, project_id: &str, base_role: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query!(
            "DELETE FROM agents WHERE project_id = $1 AND base_role = $2",
            project_id,
            base_role
        )
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    /// Update `system_prompt_extensions` on the default agent row for
    /// `(project_id, base_role)`. Tests use this to customise the auto-
    /// seeded default when they need a non-empty override to assert on
    /// without creating a whole new agent.
    pub async fn set_default_system_prompt_extensions(
        &self,
        project_id: &str,
        base_role: &str,
        extensions: &str,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let extensions_value = system_prompt_extensions_value(extensions);
        let result = sqlx::query!(
            r#"UPDATE agents SET system_prompt_extensions = $1,
                    updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE project_id = $2 AND base_role = $3 AND is_default = TRUE"#,
            extensions_value,
            project_id,
            base_role
        )
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn list_for_project(&self, query: AgentListQuery) -> Result<AgentListResult> {
        self.db.ensure_initialized().await?;

        let (where_sql, params) = build_where(&query.project_id, &query.base_role, 0);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM agents WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            total_q = total_q.bind(p.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        let limit_ph = format!("${}", params.len() + 1);
        let offset_ph = format!("${}", params.len() + 2);
        // NOTE: dynamic SQL (WHERE clause built from optional filters; uses inlined AGENT_COLUMNS projection) — compile-time check not possible
        let sql = format!(
            r#"SELECT id, project_id, name, base_role, description,
                    CASE WHEN jsonb_typeof(system_prompt_extensions) = 'string'
                         THEN system_prompt_extensions #>> '{{}}' ELSE '' END AS system_prompt_extensions,
                    model_preference, verification_command,
                    mcp_servers::text AS mcp_servers, skills::text AS skills, is_default,
                    (SELECT string_agg(h.proposed_text, E'\n\n---\n\n' ORDER BY h.created_at ASC)
                     FROM learned_prompt_history h
                     WHERE h.agent_id = agents.id
                       AND h.action IN ('keep','confirmed')
                    ) AS learned_prompt,
                    created_at, updated_at
             FROM agents WHERE {where_sql}
             ORDER BY is_default DESC, base_role ASC, name ASC
             LIMIT {limit_ph} OFFSET {offset_ph}"#
        );
        let mut role_q = sqlx::query_as::<_, Agent>(&sql);
        for p in &params {
            role_q = role_q.bind(p.clone());
        }
        let agents = role_q
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(self.db.pool())
            .await?;

        Ok(AgentListResult {
            agents,
            total_count: total,
        })
    }

    /// Compute aggregated effectiveness metrics for a role identified by its
    /// base_role→agent_type mapping. `window_days` limits session data lookback.
    pub async fn get_metrics(
        &self,
        project_id: &str,
        agent_type: &str,
        window_days: i64,
    ) -> Result<AgentMetrics> {
        self.db.ensure_initialized().await?;

        // Task-level metrics: closed tasks that had at least one session of this agent_type.
        let task_row = sqlx::query!(
            r#"SELECT
                CAST(SUM(CASE WHEN t.close_reason = 'completed' THEN 1 ELSE 0 END) AS DOUBLE PRECISION)
                    / CAST(GREATEST(1, COUNT(DISTINCT t.id)) AS DOUBLE PRECISION) AS "success_rate: f64",
                COALESCE(AVG(CAST(t.total_reopen_count AS DOUBLE PRECISION)), 0.0) AS "avg_reopens!: f64",
                COUNT(DISTINCT t.id) AS "completed_task_count!: i64"
             FROM tasks t
             WHERE t.project_id = $1
               AND t.status = 'closed'
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.agent_type = $2
               )"#,
            project_id,
            agent_type
        )
        .fetch_one(self.db.pool())
        .await
        .ok();

        // Session-level metrics: completed sessions within the lookback window.
        let session_row = sqlx::query!(
            r#"SELECT
                COALESCE(AVG(CAST(s.tokens_in + s.tokens_out AS DOUBLE PRECISION)), 0.0) AS "avg_tokens!: f64",
                COALESCE(AVG(CAST(s.tokens_in AS DOUBLE PRECISION)), 0.0) AS "avg_tokens_in!: f64",
                COALESCE(AVG(CAST(s.tokens_out AS DOUBLE PRECISION)), 0.0) AS "avg_tokens_out!: f64",
                COALESCE(AVG(
                    CASE WHEN s.ended_at IS NOT NULL
                        THEN EXTRACT(EPOCH FROM (s.ended_at::timestamp - s.started_at::timestamp))
                        ELSE NULL END
                ), 0.0) AS "avg_time_seconds!: f64",
                COALESCE(SUM((s.event_taxonomy -> 'extraction_quality' ->> 'extracted')::bigint), 0) AS "extracted!: i64",
                COALESCE(SUM((s.event_taxonomy -> 'extraction_quality' ->> 'dedup_skipped')::bigint), 0) AS "dedup_skipped!: i64",
                COALESCE(SUM((s.event_taxonomy -> 'extraction_quality' ->> 'novelty_skipped')::bigint), 0) AS "novelty_skipped!: i64",
                COALESCE(SUM((s.event_taxonomy -> 'extraction_quality' ->> 'written')::bigint), 0) AS "written!: i64",
                COALESCE(SUM((s.event_taxonomy -> 'extraction_quality' ->> 'merged')::bigint), 0) AS "merged!: i64",
                COALESCE(SUM((s.event_taxonomy -> 'extraction_quality' ->> 'downgraded')::bigint), 0) AS "downgraded!: i64",
                COALESCE(SUM((s.event_taxonomy -> 'extraction_quality' ->> 'discarded')::bigint), 0) AS "discarded!: i64"
             FROM sessions s
             JOIN tasks t ON t.id = s.task_id
             WHERE t.project_id = $1
               AND s.agent_type = $2
               AND s.status = 'completed'
               AND s.started_at >= to_char((now() at time zone 'utc') - (interval '1 day' * $3), 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#,
            project_id,
            agent_type,
            window_days as f64
        )
        .fetch_one(self.db.pool())
        .await
        .ok();

        Ok(AgentMetrics {
            success_rate: task_row
                .as_ref()
                .and_then(|r| r.success_rate)
                .unwrap_or(0.0),
            avg_reopens: task_row.as_ref().map(|r| r.avg_reopens).unwrap_or(0.0),
            completed_task_count: task_row
                .as_ref()
                .map(|r| r.completed_task_count)
                .unwrap_or(0),
            avg_tokens: session_row.as_ref().map(|r| r.avg_tokens).unwrap_or(0.0),
            avg_tokens_in: session_row.as_ref().map(|r| r.avg_tokens_in).unwrap_or(0.0),
            avg_tokens_out: session_row
                .as_ref()
                .map(|r| r.avg_tokens_out)
                .unwrap_or(0.0),
            avg_time_seconds: session_row
                .as_ref()
                .map(|r| r.avg_time_seconds)
                .unwrap_or(0.0),
            extraction_quality: ExtractionQualityMetrics {
                extracted: session_row.as_ref().map(|r| r.extracted).unwrap_or(0),
                dedup_skipped: session_row.as_ref().map(|r| r.dedup_skipped).unwrap_or(0),
                novelty_skipped: session_row.as_ref().map(|r| r.novelty_skipped).unwrap_or(0),
                written: session_row.as_ref().map(|r| r.written).unwrap_or(0),
                merged: session_row.as_ref().map(|r| r.merged).unwrap_or(0),
                downgraded: session_row.as_ref().map(|r| r.downgraded).unwrap_or(0),
                discarded: session_row.as_ref().map(|r| r.discarded).unwrap_or(0),
            },
        })
    }

    /// Return pending (action='keep', metrics_after IS NULL) history entries for
    /// all roles in the project.  These are amendments that have been applied but
    /// not yet evaluated.
    pub async fn get_pending_evaluations(
        &self,
        project_id: &str,
    ) -> Result<Vec<PendingAmendmentEvaluation>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query!(
            r#"SELECT h.id, h.agent_id, h.created_at, h.proposed_text, h.metrics_before::text AS "metrics_before"
             FROM learned_prompt_history h
             JOIN agents r ON r.id = h.agent_id
             WHERE r.project_id = $1
               AND h.action = 'keep'
               AND h.metrics_after IS NULL
             ORDER BY h.created_at ASC"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| PendingAmendmentEvaluation {
                history_id: row.id,
                agent_id: row.agent_id,
                created_at: row.created_at,
                proposed_text: row.proposed_text,
                metrics_before: row.metrics_before,
            })
            .collect())
    }

    /// Count closed tasks for a role (by agent_type) that completed (or failed)
    /// strictly after `since_timestamp` (ISO-8601 string).
    ///
    /// Returns `(completed_count, failed_count)` — tasks closed after the given
    /// timestamp whose sessions used the given agent_type.
    pub async fn count_closed_tasks_since(
        &self,
        project_id: &str,
        agent_type: &str,
        since_timestamp: &str,
    ) -> Result<(i64, i64)> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query!(
            r#"SELECT
                COALESCE(SUM(CASE WHEN t.close_reason = 'completed' THEN 1 ELSE 0 END), 0) AS "completed!: i64",
                COALESCE(SUM(CASE WHEN t.close_reason != 'completed' OR t.close_reason IS NULL THEN 1 ELSE 0 END), 0) AS "failed!: i64"
             FROM tasks t
             WHERE t.project_id = $1
               AND t.status = 'closed'
               AND t.closed_at > $2
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.agent_type = $3
               )"#,
            project_id,
            since_timestamp,
            agent_type
        )
        .fetch_one(self.db.pool())
        .await
        .ok();
        Ok(row.map(|r| (r.completed, r.failed)).unwrap_or((0, 0)))
    }

    /// Return windowed metrics for a role over tasks closed in a time range.
    ///
    /// `from_timestamp` and `to_timestamp` are ISO-8601 strings (exclusive on
    /// from, inclusive on to).  Pass `None` for open-ended bounds.
    pub async fn get_windowed_metrics(
        &self,
        project_id: &str,
        agent_type: &str,
        from_timestamp: Option<&str>,
        to_timestamp: Option<&str>,
    ) -> Result<WindowedRoleMetrics> {
        self.db.ensure_initialized().await?;

        // Default open bounds to sentinel values that cover all time.
        let from = from_timestamp.unwrap_or("1970-01-01T00:00:00.000Z");
        let to = to_timestamp.unwrap_or("9999-12-31T23:59:59.999Z");

        let row = sqlx::query!(
            r#"SELECT
                COALESCE(SUM(CASE WHEN t.close_reason = 'completed' THEN 1 ELSE 0 END), 0) AS "completed!: i64",
                COALESCE(SUM(CASE WHEN t.close_reason != 'completed' THEN 1 ELSE 0 END), 0) AS "failed!: i64",
                COALESCE(AVG(
                    CASE WHEN t.close_reason = 'completed'
                        THEN (
                            SELECT COALESCE(AVG(CAST(s.tokens_in + s.tokens_out AS DOUBLE PRECISION)), 0.0)
                            FROM sessions s
                            WHERE s.task_id = t.id AND s.agent_type = $1
                        )
                        ELSE NULL
                    END
                ), 0.0) AS "avg_tokens!: f64"
             FROM tasks t
             WHERE t.project_id = $2
               AND t.status = 'closed'
               AND t.closed_at > $3
               AND t.closed_at <= $4
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.agent_type = $5
               )"#,
            agent_type,
            project_id,
            from,
            to,
            agent_type
        )
        .fetch_one(self.db.pool())
        .await
        .ok();

        let (completed, failed, avg_tokens) = row
            .map(|r| (r.completed, r.failed, r.avg_tokens))
            .unwrap_or((0, 0, 0.0));
        let total = completed + failed;
        let success_rate = if total > 0 {
            completed as f64 / total as f64
        } else {
            0.0
        };

        Ok(WindowedRoleMetrics {
            completed_task_count: completed,
            failed_task_count: failed,
            success_rate,
            avg_tokens,
        })
    }

    /// Update a `learned_prompt_history` record with the evaluation outcome.
    ///
    /// `action` must be `'confirmed'` (metrics improved — keep the amendment) or
    /// `'discard'` (metrics did not improve — revert).
    /// `metrics_after` is a JSON snapshot of the post-amendment metrics.
    pub async fn resolve_pending_amendment(
        &self,
        history_id: &str,
        action: &str,
        metrics_after: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        let metrics_after_value: serde_json::Value =
            serde_json::from_str(metrics_after).map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json for learned_prompt_history.metrics_after: {e}"
                ))
            })?;
        sqlx::query!(
            "UPDATE learned_prompt_history
             SET action = $1, metrics_after = $2
             WHERE id = $3",
            action,
            metrics_after_value,
            history_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Mark all active amendments for an agent as discarded, effectively clearing
    /// the derived learned_prompt.
    pub async fn clear_amendments(&self, agent_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE learned_prompt_history
             SET action = 'discard'
             WHERE agent_id = $1 AND action IN ('keep','confirmed')",
            agent_id
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Append an amendment to a role's `learned_prompt` and log the proposal to
    /// `learned_prompt_history`.  The amendment is appended with a separator; it
    /// never replaces any existing content.
    ///
    /// `metrics_snapshot` is a JSON string capturing role metrics at proposal time.
    pub async fn append_learned_prompt(
        &self,
        role_id: &str,
        amendment: &str,
        metrics_snapshot: Option<&str>,
    ) -> Result<Agent> {
        self.db.ensure_initialized().await?;

        // Verify the agent exists.
        self.get(role_id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("agent not found: {role_id}")))?;

        // Insert into learned_prompt_history with action='keep' (pending eval).
        // The derived AGENT_COLUMNS query will pick it up automatically.
        let history_id = uuid::Uuid::now_v7().to_string();
        let amendment_trimmed = amendment.trim();
        let metrics_snapshot_value: Option<serde_json::Value> = match metrics_snapshot {
            Some(s) => Some(serde_json::from_str(s).map_err(|e| {
                Error::InvalidData(format!(
                    "invalid json for learned_prompt_history.metrics_before: {e}"
                ))
            })?),
            None => None,
        };
        sqlx::query!(
            "INSERT INTO learned_prompt_history
                (id, agent_id, proposed_text, action, metrics_before)
             VALUES ($1, $2, $3, 'keep', $4)",
            history_id,
            role_id,
            amendment_trimmed,
            metrics_snapshot_value
        )
        .execute(self.db.pool())
        .await?;

        // Touch updated_at so consumers see the change.
        sqlx::query!(
            r#"UPDATE agents SET updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') WHERE id = $1"#,
            role_id
        )
        .execute(self.db.pool())
        .await?;

        let updated = self.get(role_id).await?.ok_or_else(|| {
            Error::InvalidData(format!("agent not found after update: {role_id}"))
        })?;
        self.events
            .send(DjinnEventEnvelope::agent_updated(&updated));
        Ok(updated)
    }
}

fn build_where(
    project_id: &str,
    base_role: &Option<String>,
    param_offset: usize,
) -> (String, Vec<String>) {
    let mut params: Vec<String> = vec![project_id.to_owned()];
    let mut clauses: Vec<String> = vec![format!("project_id = ${}", param_offset + 1)];

    if let Some(br) = base_role {
        clauses.push(format!("base_role = ${}", param_offset + params.len() + 1));
        params.push(br.clone());
    }

    (clauses.join(" AND "), params)
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;

    use super::*;
    use crate::database::Database;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    async fn create_project(db: &Database) -> String {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let owner = "test";
        let repo_slug = format!("agent-{id}");
        sqlx::query!(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
            id,
            "test",
            owner,
            repo_slug,
        )
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_and_get_role() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRepository::new(db, EventBus::noop());

        let role = repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "DB Expert",
                    base_role: "worker",
                    description: "Database migrations specialist",
                    system_prompt_extensions: "Focus on safe migrations.",
                    model_preference: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(role.name, "DB Expert");
        assert_eq!(role.base_role, "worker");
        assert!(!role.is_default);

        let fetched = repo.get(&role.id).await.unwrap().unwrap();
        assert_eq!(fetched.id, role.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn name_uniqueness_within_project() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRepository::new(db, EventBus::noop());

        repo.create_for_project(
            &project_id,
            AgentCreateInput {
                name: "My Role",
                base_role: "worker",
                description: "",
                system_prompt_extensions: "",
                model_preference: None,
                mcp_servers: None,
                skills: None,
                is_default: false,
            },
        )
        .await
        .unwrap();

        let result = repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "My Role",
                    base_role: "planner",
                    description: "",
                    system_prompt_extensions: "",
                    model_preference: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_role() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRepository::new(db, EventBus::noop());

        let role = repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "Worker",
                    base_role: "worker",
                    description: "original",
                    system_prompt_extensions: "",
                    model_preference: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await
            .unwrap();

        let updated = repo
            .update(
                &role.id,
                AgentUpdateInput {
                    name: "Worker",
                    description: "updated",
                    system_prompt_extensions: "extra prompt",
                    model_preference: Some("claude-opus-4-6"),
                    mcp_servers: "[]",
                    skills: "[]",
                    learned_prompt: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.description, "updated");
        assert_eq!(updated.system_prompt_extensions, "extra prompt");
        assert_eq!(updated.model_preference.as_deref(), Some("claude-opus-4-6"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_with_base_role_filter() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRepository::new(db, EventBus::noop());

        for (name, base_role) in [("W1", "worker"), ("W2", "worker"), ("P1", "planner")] {
            repo.create_for_project(
                &project_id,
                AgentCreateInput {
                    name,
                    base_role,
                    description: "",
                    system_prompt_extensions: "",
                    model_preference: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await
            .unwrap();
        }

        let workers = repo
            .list_for_project(AgentListQuery {
                project_id: project_id.clone(),
                base_role: Some("worker".to_string()),
                limit: 25,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(workers.total_count, 2);
        assert_eq!(workers.agents.len(), 2);

        let all = repo
            .list_for_project(AgentListQuery {
                project_id,
                base_role: None,
                limit: 25,
                offset: 0,
            })
            .await
            .unwrap();
        assert_eq!(all.total_count, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_default_switches_default() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRepository::new(db, EventBus::noop());

        // Create two worker roles; first one is default.
        let default_role = repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "Worker A",
                    base_role: "worker",
                    description: "",
                    system_prompt_extensions: "",
                    model_preference: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: true,
                },
            )
            .await
            .unwrap();

        let specialist = repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "Worker B",
                    base_role: "worker",
                    description: "",
                    system_prompt_extensions: "",
                    model_preference: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: false,
                },
            )
            .await
            .unwrap();

        // Promote specialist to default.
        let updated = repo.set_default(&specialist.id).await.unwrap();
        assert!(updated.is_default);

        // Old default should now be cleared.
        let old = repo.get(&default_role.id).await.unwrap().unwrap();
        assert!(!old.is_default);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_default_rejected_by_db() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRepository::new(db, EventBus::noop());

        // First default worker — OK.
        repo.create_for_project(
            &project_id,
            AgentCreateInput {
                name: "Worker A",
                base_role: "worker",
                description: "",
                system_prompt_extensions: "",
                model_preference: None,
                mcp_servers: None,
                skills: None,
                is_default: true,
            },
        )
        .await
        .unwrap();

        // A second default worker in the same project/base_role should be rejected by the
        // unique partial index.
        let result = repo
            .create_for_project(
                &project_id,
                AgentCreateInput {
                    name: "Worker B",
                    base_role: "worker",
                    description: "",
                    system_prompt_extensions: "",
                    model_preference: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: true,
                },
            )
            .await;

        let error = result.expect_err("second default should violate unique partial index");
        let message = error.to_string().to_lowercase();
        // Postgres phrases a unique violation as `duplicate key value violates
        // unique constraint "<name>"`; the partial index guarding one default
        // per (project, base_role) is `uq_agents_one_default_per_base_role`.
        assert!(
            message.contains("duplicate key value violates unique constraint")
                || message.contains("uq_agents_one_default_per_base_role"),
            "unexpected error: {message}"
        );

        let defaults_rows = sqlx::query!(
            r#"SELECT name, is_default AS "is_default!: i64" FROM agents WHERE project_id = $1 AND base_role = 'worker' ORDER BY name"#,
            project_id
        )
        .fetch_all(repo.db.pool())
        .await
        .unwrap();
        let defaults: Vec<(String, i64)> = defaults_rows
            .into_iter()
            .map(|r| (r.name, r.is_default))
            .collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0], ("Worker A".to_string(), 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_emits_event() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(Vec::new()));
        let bus = EventBus::new({
            let captured = captured.clone();
            move |ev| captured.lock().unwrap().push(ev)
        });
        let db = test_db();
        let project_id = create_project(&db).await;
        let repo = AgentRepository::new(db, bus);

        repo.create_for_project(
            &project_id,
            AgentCreateInput {
                name: "Event Role",
                base_role: "worker",
                description: "",
                system_prompt_extensions: "",
                model_preference: None,
                mcp_servers: None,
                skills: None,
                is_default: false,
            },
        )
        .await
        .unwrap();

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].entity_type, "agent");
        assert_eq!(events[0].action, "created");
    }

    // ── Learned prompt lifecycle regression tests ───────────────────────────

    /// Helper: create a worker agent and return its id.
    async fn create_worker_agent(db: &Database, project_id: &str, name: &str) -> Agent {
        let repo = AgentRepository::new(db.clone(), EventBus::noop());
        repo.create_for_project(
            project_id,
            AgentCreateInput {
                name,
                base_role: "worker",
                description: "test worker",
                system_prompt_extensions: "",
                model_preference: None,
                mcp_servers: None,
                skills: None,
                is_default: false,
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_learned_prompt_records_history_and_derives_active() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w1").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        // Before amendment: learned_prompt should be None.
        let before = repo.get(&agent.id).await.unwrap().unwrap();
        assert!(before.learned_prompt.is_none());

        // Append an amendment.
        let updated = repo
            .append_learned_prompt(&agent.id, "Always write tests first.", None)
            .await
            .unwrap();

        // Derived learned_prompt should now contain the amendment text.
        let lp = updated.learned_prompt.as_deref().unwrap();
        assert!(
            lp.contains("Always write tests first."),
            "derived learned_prompt should contain the amendment: {lp:?}"
        );

        // History should have exactly one entry.
        let history = repo.get_history(&agent.id).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].proposed_text, "Always write tests first.");
        assert_eq!(history[0].action, "keep");
        assert!(history[0].metrics_after.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_amendments_concat_in_derived_prompt() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w2").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        repo.append_learned_prompt(&agent.id, "First amendment.", None)
            .await
            .unwrap();
        let updated = repo
            .append_learned_prompt(&agent.id, "Second amendment.", None)
            .await
            .unwrap();

        let lp = updated.learned_prompt.as_deref().unwrap();
        assert!(lp.contains("First amendment."));
        assert!(lp.contains("Second amendment."));

        let history = repo.get_history(&agent.id).await.unwrap();
        assert_eq!(history.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_pending_evaluations_returns_pending_entries() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w3").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        // Before any amendment, no pending evaluations.
        let pending = repo.get_pending_evaluations(&project_id).await.unwrap();
        assert!(pending.is_empty());

        // Append an amendment.
        repo.append_learned_prompt(&agent.id, "Pending amendment.", None)
            .await
            .unwrap();

        let pending = repo.get_pending_evaluations(&project_id).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].agent_id, agent.id);
        assert_eq!(pending[0].proposed_text, "Pending amendment.");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_pending_amendment_confirmed_keeps_in_derived_prompt() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w4").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        repo.append_learned_prompt(&agent.id, "Good amendment.", None)
            .await
            .unwrap();

        let pending = repo.get_pending_evaluations(&project_id).await.unwrap();
        assert_eq!(pending.len(), 1);

        let metrics_json = serde_json::json!({
            "success_rate": 0.85,
            "avg_tokens": 900.0,
            "completed_task_count": 25,
            "failed_task_count": 2,
        })
        .to_string();

        // Resolve as confirmed.
        repo.resolve_pending_amendment(&pending[0].history_id, "confirmed", &metrics_json)
            .await
            .unwrap();

        // Should no longer be pending.
        let pending_after = repo.get_pending_evaluations(&project_id).await.unwrap();
        assert!(pending_after.is_empty());

        // Derived prompt should still contain the amendment (action='confirmed' is active).
        let agent_after = repo.get(&agent.id).await.unwrap().unwrap();
        let lp = agent_after.learned_prompt.as_deref().unwrap();
        assert!(lp.contains("Good amendment."));

        // History should show 'confirmed' with metrics_after populated.
        let history = repo.get_history(&agent.id).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].action, "confirmed");
        assert!(history[0].metrics_after.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_pending_amendment_discard_removes_from_derived_prompt() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w5").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        repo.append_learned_prompt(&agent.id, "Bad amendment.", None)
            .await
            .unwrap();

        // Confirm it's in the derived prompt.
        let before = repo.get(&agent.id).await.unwrap().unwrap();
        assert!(
            before
                .learned_prompt
                .as_deref()
                .unwrap()
                .contains("Bad amendment.")
        );

        let pending = repo.get_pending_evaluations(&project_id).await.unwrap();
        assert_eq!(pending.len(), 1);

        let metrics_json = serde_json::json!({
            "success_rate": 0.50,
            "avg_tokens": 1200.0,
            "completed_task_count": 20,
            "failed_task_count": 10,
        })
        .to_string();

        // Resolve as discard.
        repo.resolve_pending_amendment(&pending[0].history_id, "discard", &metrics_json)
            .await
            .unwrap();

        // Derived prompt should no longer contain the discarded amendment.
        let after = repo.get(&agent.id).await.unwrap().unwrap();
        assert!(
            after.learned_prompt.is_none(),
            "derived prompt should be None after discard: {:?}",
            after.learned_prompt
        );

        // History should show 'discard' with metrics_after populated.
        let history = repo.get_history(&agent.id).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].action, "discard");
        assert!(history[0].metrics_after.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clear_amendments_discards_all_active_amendments() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w6").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        // Append two amendments.
        repo.append_learned_prompt(&agent.id, "Amendment A.", None)
            .await
            .unwrap();
        repo.append_learned_prompt(&agent.id, "Amendment B.", None)
            .await
            .unwrap();

        // Both should be in the derived prompt.
        let before = repo.get(&agent.id).await.unwrap().unwrap();
        let lp = before.learned_prompt.as_deref().unwrap();
        assert!(lp.contains("Amendment A."));
        assert!(lp.contains("Amendment B."));

        // Clear all amendments.
        repo.clear_amendments(&agent.id).await.unwrap();

        // Derived prompt should now be None.
        let after = repo.get(&agent.id).await.unwrap().unwrap();
        assert!(
            after.learned_prompt.is_none(),
            "derived prompt should be None after clear_amendments: {:?}",
            after.learned_prompt
        );

        // History should show both as 'discard'.
        let history = repo.get_history(&agent.id).await.unwrap();
        assert_eq!(history.len(), 2);
        for entry in &history {
            assert_eq!(entry.action, "discard");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_learned_prompt_with_metrics_snapshot_round_trips() {
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w7").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        let metrics_json = serde_json::json!({
            "success_rate": 0.72,
            "avg_tokens": 1100.0,
            "completed_task_count": 15,
            "failed_task_count": 3,
        })
        .to_string();

        repo.append_learned_prompt(&agent.id, "Amendment with metrics.", Some(&metrics_json))
            .await
            .unwrap();

        let history = repo.get_history(&agent.id).await.unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].metrics_before.is_some());
        let before_json = history[0].metrics_before.as_ref().unwrap();
        assert!(before_json.contains("0.72"));
        assert!(before_json.contains("1100"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn discard_revert_learns_from_confirmed_and_keeps_confirmed() {
        // Scenario: Two amendments, first is confirmed, second is discarded.
        // The derived prompt should only include the confirmed one.
        let db = test_db();
        let project_id = create_project(&db).await;
        let agent = create_worker_agent(&db, &project_id, "w8").await;
        let repo = AgentRepository::new(db, EventBus::noop());

        // First amendment: will be confirmed.
        repo.append_learned_prompt(&agent.id, "Confirmed rule.", None)
            .await
            .unwrap();

        let pending = repo.get_pending_evaluations(&project_id).await.unwrap();
        let metrics_json = serde_json::json!({
            "success_rate": 0.90,
            "avg_tokens": 800.0,
            "completed_task_count": 25,
            "failed_task_count": 1,
        })
        .to_string();
        repo.resolve_pending_amendment(&pending[0].history_id, "confirmed", &metrics_json)
            .await
            .unwrap();

        // Second amendment: will be discarded.
        repo.append_learned_prompt(&agent.id, "Discarded rule.", None)
            .await
            .unwrap();

        let pending2 = repo.get_pending_evaluations(&project_id).await.unwrap();
        let metrics_json2 = serde_json::json!({
            "success_rate": 0.60,
            "avg_tokens": 1200.0,
            "completed_task_count": 20,
            "failed_task_count": 8,
        })
        .to_string();
        repo.resolve_pending_amendment(&pending2[0].history_id, "discard", &metrics_json2)
            .await
            .unwrap();

        // Derived prompt should contain only the confirmed amendment.
        let agent_after = repo.get(&agent.id).await.unwrap().unwrap();
        let lp = agent_after.learned_prompt.as_deref().unwrap();
        assert!(lp.contains("Confirmed rule."));
        assert!(!lp.contains("Discarded rule."));

        // History: two entries, first confirmed, second discarded.
        let history = repo.get_history(&agent.id).await.unwrap();
        assert_eq!(history.len(), 2);
        // History is newest first.
        assert_eq!(history[0].action, "discard");
        assert_eq!(history[0].proposed_text, "Discarded rule.");
        assert_eq!(history[1].action, "confirmed");
        assert_eq!(history[1].proposed_text, "Confirmed rule.");
    }
}
