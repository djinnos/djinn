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
                model_preference,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
                created_at AS "created_at!", updated_at AS "updated_at!"
             FROM agents
             ORDER BY project_id ASC, is_default DESC, base_role ASC, name ASC"#
        )
        .fetch_all(self.db.pool())
        .await?)
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
                model_preference,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
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
                model_preference,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
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
                model_preference,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
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
                model_preference,
                mcp_servers::text AS "mcp_servers!", skills::text AS "skills!",
                is_default AS "is_default!: bool",
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
                 mcp_servers = $5, skills = $6,
                 updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $7"#,
            input.name,
            input.description,
            system_prompt_value,
            input.model_preference,
            mcp_servers_value,
            skills_value,
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
                    model_preference,
                    mcp_servers::text AS mcp_servers, skills::text AS skills, is_default,
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
        //
        // NOTE: errors are propagated (via `?`) rather than swallowed. The
        // previous implementation ended both queries with `.await.ok()`, which
        // turned ANY SQL error into a silent all-zero metric row — in
        // production this made 4 of 5 agents report zero session metrics while
        // clearly having sessions, with nothing surfaced to the operator.
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
        .await?;

        // Session-level metrics: completed sessions within the lookback window.
        //
        // Robustness notes:
        //   * `avg_time_seconds` — `EXTRACT(EPOCH FROM …)` yields Postgres
        //     NUMERIC, and `AVG(NUMERIC)` stays NUMERIC. Decoding NUMERIC bytes
        //     as `f64` produced denormal garbage (e.g. 6.95e-309). The AVG is
        //     therefore cast explicitly to DOUBLE PRECISION so sqlx decodes it
        //     correctly.
        //   * extraction-quality counters — the raw JSON text was cast straight
        //     to `::bigint`, which throws `invalid input syntax for type
        //     bigint` on empty strings or non-numeric values. Each value is now
        //     guarded by an integer regex so malformed entries contribute NULL
        //     (ignored by SUM) instead of erroring the whole query.
        let session_row = sqlx::query!(
            r#"SELECT
                COALESCE(AVG(CAST(s.tokens_in + s.tokens_out AS DOUBLE PRECISION)), 0.0) AS "avg_tokens!: f64",
                COALESCE(AVG(CAST(s.tokens_in AS DOUBLE PRECISION)), 0.0) AS "avg_tokens_in!: f64",
                COALESCE(AVG(CAST(s.tokens_out AS DOUBLE PRECISION)), 0.0) AS "avg_tokens_out!: f64",
                COALESCE(CAST(AVG(
                    CASE WHEN s.ended_at IS NOT NULL
                        THEN EXTRACT(EPOCH FROM (s.ended_at::timestamp - s.started_at::timestamp))
                        ELSE NULL END
                ) AS DOUBLE PRECISION), 0.0) AS "avg_time_seconds!: f64",
                CAST(COALESCE(SUM(CASE WHEN (s.event_taxonomy -> 'extraction_quality' ->> 'extracted') ~ '^-?[0-9]+$'
                    THEN (s.event_taxonomy -> 'extraction_quality' ->> 'extracted')::bigint END), 0) AS BIGINT) AS "extracted!: i64",
                CAST(COALESCE(SUM(CASE WHEN (s.event_taxonomy -> 'extraction_quality' ->> 'dedup_skipped') ~ '^-?[0-9]+$'
                    THEN (s.event_taxonomy -> 'extraction_quality' ->> 'dedup_skipped')::bigint END), 0) AS BIGINT) AS "dedup_skipped!: i64",
                CAST(COALESCE(SUM(CASE WHEN (s.event_taxonomy -> 'extraction_quality' ->> 'novelty_skipped') ~ '^-?[0-9]+$'
                    THEN (s.event_taxonomy -> 'extraction_quality' ->> 'novelty_skipped')::bigint END), 0) AS BIGINT) AS "novelty_skipped!: i64",
                CAST(COALESCE(SUM(CASE WHEN (s.event_taxonomy -> 'extraction_quality' ->> 'written') ~ '^-?[0-9]+$'
                    THEN (s.event_taxonomy -> 'extraction_quality' ->> 'written')::bigint END), 0) AS BIGINT) AS "written!: i64",
                CAST(COALESCE(SUM(CASE WHEN (s.event_taxonomy -> 'extraction_quality' ->> 'merged') ~ '^-?[0-9]+$'
                    THEN (s.event_taxonomy -> 'extraction_quality' ->> 'merged')::bigint END), 0) AS BIGINT) AS "merged!: i64",
                CAST(COALESCE(SUM(CASE WHEN (s.event_taxonomy -> 'extraction_quality' ->> 'downgraded') ~ '^-?[0-9]+$'
                    THEN (s.event_taxonomy -> 'extraction_quality' ->> 'downgraded')::bigint END), 0) AS BIGINT) AS "downgraded!: i64",
                CAST(COALESCE(SUM(CASE WHEN (s.event_taxonomy -> 'extraction_quality' ->> 'discarded') ~ '^-?[0-9]+$'
                    THEN (s.event_taxonomy -> 'extraction_quality' ->> 'discarded')::bigint END), 0) AS BIGINT) AS "discarded!: i64"
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
        .await?;

        Ok(AgentMetrics {
            success_rate: task_row.success_rate.unwrap_or(0.0),
            avg_reopens: task_row.avg_reopens,
            completed_task_count: task_row.completed_task_count,
            avg_tokens: session_row.avg_tokens,
            avg_tokens_in: session_row.avg_tokens_in,
            avg_tokens_out: session_row.avg_tokens_out,
            avg_time_seconds: session_row.avg_time_seconds,
            extraction_quality: ExtractionQualityMetrics {
                extracted: session_row.extracted,
                dedup_skipped: session_row.dedup_skipped,
                novelty_skipped: session_row.novelty_skipped,
                written: session_row.written,
                merged: session_row.merged,
                downgraded: session_row.downgraded,
                discarded: session_row.discarded,
            },
        })
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

    // ── get_metrics robustness (defect: silent all-zero metrics) ─────────────

    /// Insert a closed+completed task plus one completed worker session on it,
    /// with a caller-controlled duration and (optionally malformed)
    /// `event_taxonomy`. `started_at`/`ended_at` are set to a recent instant so
    /// the session falls inside any reasonable lookback window.
    async fn insert_completed_worker_session(
        db: &Database,
        project_id: &str,
        duration_seconds: i64,
        event_taxonomy: Option<&str>,
    ) {
        let task_id = uuid::Uuid::now_v7().to_string();
        let creator = crate::repositories::test_support::seed_test_user(db).await;
        // UUIDv7 shares a time-ordered prefix, so use the random tail for a
        // collision-free short_id (project_id + short_id is UNIQUE).
        let short = format!("t{}", &task_id[task_id.len() - 12..]);
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, \
              status, close_reason, total_reopen_count, \
              labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ($1, $2, $3, 'metrics-task', '', '', \
                     'closed', 'completed', 0, \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $4)",
        )
        .bind(&task_id)
        .bind(project_id)
        .bind(&short)
        .bind(&creator)
        .execute(db.pool())
        .await
        .unwrap();

        let session_id = uuid::Uuid::now_v7().to_string();
        // `duration_seconds` is a repository-controlled integer literal (never
        // user input); inlining it keeps the timestamp arithmetic in SQL.
        let sql = format!(
            "INSERT INTO sessions \
             (id, project_id, task_id, model_id, agent_type, status, \
              started_at, ended_at, tokens_in, tokens_out, event_taxonomy) \
             VALUES ($1, $2, $3, 'model-x', 'worker', 'completed', \
                 to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                 to_char((now() at time zone 'utc') + (interval '1 second' * {duration_seconds}), \
                         'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                 100, 50, $4::jsonb)"
        );
        sqlx::query(&sql)
            .bind(&session_id)
            .bind(project_id)
            .bind(&task_id)
            .bind(event_taxonomy)
            .execute(db.pool())
            .await
            .unwrap();
    }

    /// The session query must survive rows whose `event_taxonomy` is NULL,
    /// lacks `extraction_quality`, or carries empty / non-numeric counter
    /// values — previously any such row made the whole `::bigint` cast throw,
    /// and `.await.ok()` silently zeroed every metric. It must now succeed and
    /// sum only the well-formed numeric values.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_robust_to_missing_and_nonnumeric_extraction_quality() {
        let db = test_db();
        let project_id = create_project(&db).await;

        // Valid numeric counters (string-encoded).
        insert_completed_worker_session(
            &db,
            &project_id,
            10,
            Some(r#"{"extraction_quality":{"extracted":"3","written":"1"}}"#),
        )
        .await;
        // Valid numeric counters (JSON numbers, not strings).
        insert_completed_worker_session(
            &db,
            &project_id,
            10,
            Some(r#"{"extraction_quality":{"extracted":2,"merged":5}}"#),
        )
        .await;
        // Non-numeric value — would have thrown `invalid input syntax for
        // type bigint: "abc"`.
        insert_completed_worker_session(
            &db,
            &project_id,
            10,
            Some(r#"{"extraction_quality":{"extracted":"abc"}}"#),
        )
        .await;
        // Empty-string value — would have thrown `invalid input syntax for
        // type bigint: ""`.
        insert_completed_worker_session(
            &db,
            &project_id,
            10,
            Some(r#"{"extraction_quality":{"extracted":""}}"#),
        )
        .await;
        // No extraction_quality key at all.
        insert_completed_worker_session(&db, &project_id, 10, Some(r#"{"foo":"bar"}"#)).await;
        // NULL event_taxonomy.
        insert_completed_worker_session(&db, &project_id, 10, None).await;

        let repo = AgentRepository::new(db, EventBus::noop());
        let metrics = repo
            .get_metrics(&project_id, "worker", 3650)
            .await
            .expect("get_metrics must not error on malformed event_taxonomy");

        // Only the two well-formed rows contribute.
        assert_eq!(metrics.extraction_quality.extracted, 5, "3 + 2");
        assert_eq!(metrics.extraction_quality.written, 1);
        assert_eq!(metrics.extraction_quality.merged, 5);
        assert_eq!(metrics.extraction_quality.discarded, 0);
        // All six sessions are counted for token/duration averages.
        assert!((metrics.avg_tokens - 150.0).abs() < 1e-6);
        assert_eq!(metrics.completed_task_count, 6);
    }

    /// `avg_time_seconds` must decode as a real number. `EXTRACT(EPOCH …)`
    /// returns NUMERIC and `AVG(NUMERIC)` stays NUMERIC; decoding those bytes
    /// as `f64` previously yielded denormal garbage (~6.95e-309). The explicit
    /// DOUBLE PRECISION cast makes it sane.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_avg_time_seconds_is_sane() {
        let db = test_db();
        let project_id = create_project(&db).await;

        insert_completed_worker_session(&db, &project_id, 30, None).await;
        insert_completed_worker_session(&db, &project_id, 50, None).await;

        let repo = AgentRepository::new(db, EventBus::noop());
        let metrics = repo.get_metrics(&project_id, "worker", 3650).await.unwrap();

        // Mean of 30s and 50s = 40s. Assert it is a normal, plausible value
        // (the denormal-garbage bug produced values near zero, ~1e-309).
        assert!(
            (metrics.avg_time_seconds - 40.0).abs() < 1.0,
            "avg_time_seconds should be ~40, got {}",
            metrics.avg_time_seconds
        );
        assert!(
            metrics.avg_time_seconds > 1.0,
            "avg_time_seconds must not be a denormal near-zero value, got {}",
            metrics.avg_time_seconds
        );
    }
}
