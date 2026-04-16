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
    pub verification_command: Option<&'a str>,
    pub mcp_servers: Option<&'a str>,
    pub skills: Option<&'a str>,
    pub is_default: bool,
}

pub struct AgentUpdateInput<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub system_prompt_extensions: &'a str,
    pub model_preference: Option<&'a str>,
    pub verification_command: Option<&'a str>,
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
    /// Fraction of closed tasks with zero verification failures (0.0–1.0).
    pub verification_pass_rate: f64,
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

/// Standard SELECT projection for Agent queries.  `learned_prompt` is derived
/// from active `learned_prompt_history` rows rather than the stale text column.
///
/// Implemented as a `macro_rules!` that expands into a full
/// `sqlx::query_as!(Agent, "...", $($bind),*)` call so callers get
/// compile-time SQL checking.  `query_as!` requires a string-literal SQL
/// argument, which a `const &str` does not satisfy.
/// Inline AGENT_COLUMNS projection for each `query_as!(Agent, ...)` call site.
/// `query_as!` requires a string-literal SQL argument; `concat!()` doesn't
/// satisfy it (verified during batch 4).  Each caller therefore passes the
/// full SELECT body as a single raw string literal.

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
            r#"SELECT id, project_id, `name`, base_role, description,
                system_prompt_extensions, model_preference, verification_command,
                mcp_servers, skills,
                is_default AS "is_default!: bool",
                (SELECT GROUP_CONCAT(h.proposed_text ORDER BY h.created_at ASC SEPARATOR '\n\n---\n\n')
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at, updated_at
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
            r#"SELECT id, proposed_text, action AS "action!", metrics_before, metrics_after, created_at
                 FROM learned_prompt_history
                 WHERE agent_id = ?
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

    /// Set a role's `learned_prompt` to NULL and emit an update event.
    pub async fn clear_learned_prompt(&self, role_id: &str) -> Result<Agent> {
        self.db.ensure_initialized().await?;
        sqlx::query!(
            "UPDATE agents
             SET learned_prompt = NULL,
                 updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            role_id
        )
        .execute(self.db.pool())
        .await?;

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
            r#"SELECT id, project_id, `name`, base_role, description,
                system_prompt_extensions, model_preference, verification_command,
                mcp_servers, skills,
                is_default AS "is_default!: bool",
                (SELECT GROUP_CONCAT(h.proposed_text ORDER BY h.created_at ASC SEPARATOR '\n\n---\n\n')
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at, updated_at
             FROM agents WHERE id = ?"#,
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
            r#"SELECT id, project_id, `name`, base_role, description,
                system_prompt_extensions, model_preference, verification_command,
                mcp_servers, skills,
                is_default AS "is_default!: bool",
                (SELECT GROUP_CONCAT(h.proposed_text ORDER BY h.created_at ASC SEPARATOR '\n\n---\n\n')
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at, updated_at
             FROM agents
             WHERE project_id = ? AND base_role = ? AND is_default = 1 LIMIT 1"#,
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
            r#"SELECT id, project_id, `name`, base_role, description,
                system_prompt_extensions, model_preference, verification_command,
                mcp_servers, skills,
                is_default AS "is_default!: bool",
                (SELECT GROUP_CONCAT(h.proposed_text ORDER BY h.created_at ASC SEPARATOR '\n\n---\n\n')
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at, updated_at
             FROM agents WHERE project_id = ? AND `name` = ?"#,
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
            r#"SELECT id, project_id, `name`, base_role, description,
                system_prompt_extensions, model_preference, verification_command,
                mcp_servers, skills,
                is_default AS "is_default!: bool",
                (SELECT GROUP_CONCAT(h.proposed_text ORDER BY h.created_at ASC SEPARATOR '\n\n---\n\n')
                 FROM learned_prompt_history h
                 WHERE h.agent_id = agents.id
                   AND h.action IN ('keep','confirmed')
                ) AS learned_prompt,
                created_at, updated_at
             FROM agents
             WHERE project_id = ? ORDER BY is_default DESC, base_role ASC, `name` ASC"#,
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
        let is_default_int = input.is_default as i64;
        sqlx::query!(
            "INSERT INTO agents (
                id, project_id, `name`, base_role, description,
                system_prompt_extensions, model_preference, verification_command,
                mcp_servers, skills, is_default
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            id,
            project_id,
            input.name,
            input.base_role,
            input.description,
            input.system_prompt_extensions,
            input.model_preference,
            input.verification_command,
            mcp_servers,
            skills,
            is_default_int
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
        sqlx::query!(
            "UPDATE agents
             SET `name` = ?, description = ?, system_prompt_extensions = ?,
                 model_preference = ?, verification_command = ?,
                 mcp_servers = ?, skills = ?, learned_prompt = ?,
                 updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            input.name,
            input.description,
            input.system_prompt_extensions,
            input.model_preference,
            input.verification_command,
            input.mcp_servers,
            input.skills,
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
            "UPDATE agents SET is_default = 0
             WHERE project_id = ? AND base_role = ? AND is_default = 1",
            role.project_id,
            role.base_role
        )
        .execute(self.db.pool())
        .await?;

        // Set this role as default.
        sqlx::query!(
            "UPDATE agents SET is_default = 1,
                     updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
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
        sqlx::query!("DELETE FROM agents WHERE id = ?", id)
            .execute(self.db.pool())
            .await?;
        self.events
            .send(DjinnEventEnvelope::agent_deleted(id, project_id));
        Ok(())
    }

    pub async fn list_for_project(&self, query: AgentListQuery) -> Result<AgentListResult> {
        self.db.ensure_initialized().await?;

        let (where_sql, params) = build_where(&query.project_id, &query.base_role);

        // NOTE: dynamic SQL (WHERE clause built from optional filters) — compile-time check not possible
        let total_sql = format!("SELECT COUNT(*) FROM agents WHERE {where_sql}");
        let mut total_q = sqlx::query_scalar::<_, i64>(&total_sql);
        for p in &params {
            total_q = total_q.bind(p.clone());
        }
        let total = total_q.fetch_one(self.db.pool()).await?;

        // NOTE: dynamic SQL (WHERE clause built from optional filters; uses inlined AGENT_COLUMNS projection) — compile-time check not possible
        let sql = format!(
            "SELECT id, project_id, `name`, base_role, description, \
                    system_prompt_extensions, model_preference, verification_command, \
                    mcp_servers, skills, is_default, \
                    (SELECT GROUP_CONCAT(h.proposed_text ORDER BY h.created_at ASC SEPARATOR '\n\n---\n\n') \
                     FROM learned_prompt_history h \
                     WHERE h.agent_id = agents.id \
                       AND h.action IN ('keep','confirmed') \
                    ) AS learned_prompt, \
                    created_at, updated_at \
             FROM agents WHERE {where_sql} \
             ORDER BY is_default DESC, base_role ASC, name ASC \
             LIMIT ? OFFSET ?"
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
                CAST(SUM(CASE WHEN t.close_reason = 'completed' THEN 1 ELSE 0 END) AS DOUBLE)
                    / CAST(GREATEST(1, COUNT(DISTINCT t.id)) AS DOUBLE) AS "success_rate: f64",
                COALESCE(AVG(CAST(t.total_reopen_count AS DOUBLE)), 0.0) AS "avg_reopens!: f64",
                CAST(SUM(CASE WHEN t.total_verification_failure_count = 0 THEN 1 ELSE 0 END) AS DOUBLE)
                    / CAST(GREATEST(1, COUNT(DISTINCT t.id)) AS DOUBLE) AS "verification_pass_rate: f64",
                COUNT(DISTINCT t.id) AS "completed_task_count!: i64"
             FROM tasks t
             WHERE t.project_id = ?
               AND t.`status` = 'closed'
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.agent_type = ?
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
                COALESCE(AVG(CAST(s.tokens_in + s.tokens_out AS DOUBLE)), 0.0) AS "avg_tokens!: f64",
                COALESCE(AVG(CAST(s.tokens_in AS DOUBLE)), 0.0) AS "avg_tokens_in!: f64",
                COALESCE(AVG(CAST(s.tokens_out AS DOUBLE)), 0.0) AS "avg_tokens_out!: f64",
                COALESCE(AVG(
                    CASE WHEN s.ended_at IS NOT NULL
                        THEN CAST(TIMESTAMPDIFF(MICROSECOND, s.started_at, s.ended_at) / 1000000.0 AS DOUBLE)
                        ELSE NULL END
                ), 0.0) AS "avg_time_seconds!: f64",
                COALESCE(SUM(CAST(JSON_EXTRACT(s.event_taxonomy, '$.extraction_quality.extracted') AS SIGNED)), 0) AS "extracted!: i64",
                COALESCE(SUM(CAST(JSON_EXTRACT(s.event_taxonomy, '$.extraction_quality.dedup_skipped') AS SIGNED)), 0) AS "dedup_skipped!: i64",
                COALESCE(SUM(CAST(JSON_EXTRACT(s.event_taxonomy, '$.extraction_quality.novelty_skipped') AS SIGNED)), 0) AS "novelty_skipped!: i64",
                COALESCE(SUM(CAST(JSON_EXTRACT(s.event_taxonomy, '$.extraction_quality.written') AS SIGNED)), 0) AS "written!: i64",
                COALESCE(SUM(CAST(JSON_EXTRACT(s.event_taxonomy, '$.extraction_quality.merged') AS SIGNED)), 0) AS "merged!: i64",
                COALESCE(SUM(CAST(JSON_EXTRACT(s.event_taxonomy, '$.extraction_quality.downgraded') AS SIGNED)), 0) AS "downgraded!: i64",
                COALESCE(SUM(CAST(JSON_EXTRACT(s.event_taxonomy, '$.extraction_quality.discarded') AS SIGNED)), 0) AS "discarded!: i64"
             FROM sessions s
             JOIN tasks t ON t.id = s.task_id
             WHERE t.project_id = ?
               AND s.agent_type = ?
               AND s.`status` = 'completed'
               AND s.started_at >= DATE_SUB(NOW(3), INTERVAL ? DAY)"#,
            project_id,
            agent_type,
            window_days
        )
        .fetch_one(self.db.pool())
        .await
        .ok();

        Ok(AgentMetrics {
            success_rate: task_row.as_ref().and_then(|r| r.success_rate).unwrap_or(0.0),
            avg_reopens: task_row.as_ref().map(|r| r.avg_reopens).unwrap_or(0.0),
            verification_pass_rate: task_row
                .as_ref()
                .and_then(|r| r.verification_pass_rate)
                .unwrap_or(0.0),
            completed_task_count: task_row.as_ref().map(|r| r.completed_task_count).unwrap_or(0),
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
            "SELECT h.id, h.agent_id, h.created_at, h.proposed_text, h.metrics_before
             FROM learned_prompt_history h
             JOIN agents r ON r.id = h.agent_id
             WHERE r.project_id = ?
               AND h.action = 'keep'
               AND h.metrics_after IS NULL
             ORDER BY h.created_at ASC",
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
             WHERE t.project_id = ?
               AND t.`status` = 'closed'
               AND t.closed_at > ?
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.agent_type = ?
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
                            SELECT COALESCE(AVG(CAST(s.tokens_in + s.tokens_out AS DOUBLE)), 0.0)
                            FROM sessions s
                            WHERE s.task_id = t.id AND s.agent_type = ?
                        )
                        ELSE NULL
                    END
                ), 0.0) AS "avg_tokens!: f64"
             FROM tasks t
             WHERE t.project_id = ?
               AND t.`status` = 'closed'
               AND t.closed_at > ?
               AND t.closed_at <= ?
               AND EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.agent_type = ?
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
        sqlx::query!(
            "UPDATE learned_prompt_history
             SET action = ?, metrics_after = ?
             WHERE id = ?",
            action,
            metrics_after,
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
             WHERE agent_id = ? AND action IN ('keep','confirmed')",
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
        sqlx::query!(
            "INSERT INTO learned_prompt_history
                (id, agent_id, proposed_text, action, metrics_before)
             VALUES (?, ?, ?, 'keep', ?)",
            history_id,
            role_id,
            amendment_trimmed,
            metrics_snapshot
        )
        .execute(self.db.pool())
        .await?;

        // Touch updated_at so consumers see the change.
        sqlx::query!(
            "UPDATE agents SET updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ') WHERE id = ?",
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

fn build_where(project_id: &str, base_role: &Option<String>) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = vec!["project_id = ?".to_owned()];
    let mut params: Vec<String> = vec![project_id.to_owned()];

    if let Some(br) = base_role {
        clauses.push("base_role = ?".to_owned());
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
        let path = format!("/tmp/test-{id}");
        sqlx::query!(
            "INSERT INTO projects (id, name, path, verification_rules) VALUES (?, ?, ?, ?)",
            id,
            "test",
            path,
            "[]"
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
                    verification_command: None,
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
                verification_command: None,
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
                    verification_command: None,
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
                    verification_command: None,
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
                    verification_command: Some("cargo test"),
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
                    verification_command: None,
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
                    verification_command: None,
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
                    verification_command: None,
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
                verification_command: None,
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
                    verification_command: None,
                    mcp_servers: None,
                    skills: None,
                    is_default: true,
                },
            )
            .await;

        let error = result.expect_err("second default should violate unique partial index");
        let message = error.to_string().to_lowercase();
        // Accept either the legacy SQLite message or the MySQL/Dolt duplicate-key
        // message. Dolt phrases it as "duplicate unique key given"; vanilla
        // MySQL says "duplicate entry". Both flows still point at the partial
        // unique index `uq_agents_project_default_base`.
        assert!(
            message.contains("unique constraint failed: agents.project_id, agents.base_role")
                || message.contains("duplicate entry")
                || message.contains("duplicate unique key")
                || message.contains("uq_agents_project_default_base"),
            "unexpected error: {message}"
        );

        let defaults_rows = sqlx::query!(
            r#"SELECT `name`, is_default AS "is_default!: i64" FROM agents WHERE project_id = ? AND base_role = 'worker' ORDER BY `name`"#,
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
                verification_command: None,
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
}
