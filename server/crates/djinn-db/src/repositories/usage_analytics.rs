// ── Usage analytics repository ─────────────────────────────────────────────
//
// Provides aggregate queries over the `sessions` table (plus dimension joins)
// for the admin usage analytics dashboard.  All day bucketing uses
// `substring(s.started_at, 1, 10)` — no `date_trunc` or `generate_series`.
//
// Cost-basis semantics (migration 83): every session carries a `cost_basis`
// column with values `'actual'`, `'projected'`, or `'unpriced'`.
//
//   - `actual_spend_usd`: sum of `cost_usd` where `cost_basis = 'actual'`.
//     Real API-key spend.
//   - `projected_usd`: sum of `cost_usd` where `cost_basis = 'projected'`.
//     List-rate projection for subscription/coding-plan sessions.
//   - `unpriced_session_count`: sessions with `cost_basis = 'unpriced'`,
//     excluded from both dollar figures but counted visibly.
//   - `total_cost_usd` / `cost` / `spend_usd`: **deprecated** list-rate
//     sum across all priced bases (actual + projected). Retained for
//     backward compatibility; new UI surfaces should not use them.
//
// The dashboard consumes a single response shaped exactly like the frontend
// contract:
//   - a multi-dimensional time series (per day × model × project × agent) so
//     the Overview tab can group spend client-side;
//   - four entity breakdowns (user / project / proposal / task) aggregated
//     across the whole window;
//   - worker-scoped per-model effectiveness;
//   - a project × model matrix with per-cell outcome metrics.

use sqlx::Row;

use crate::Result;
use crate::database::Database;

// ── Model effectiveness / project-model matrix types ─────────────────────

/// Per-model effectiveness row computed over **worker sessions only**.
///
/// Completed-task attribution uses *shared-credit*: every model that ran at
/// least one worker session on a completed task receives credit for that task.
/// This means the sum of `shared_credit_completed_task_count` across models
/// can exceed the actual number of completed tasks when multiple models worked
/// on the same task.  UI code should label this field accordingly.
#[derive(Clone, Debug, Default)]
pub struct ModelEffectivenessRow {
    pub model_id: String,
    pub sessions: i64,
    /// Real API-key spend in USD. `None` when no `actual` sessions exist.
    pub actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost in USD. `None` when no
    /// `projected` sessions exist.
    pub projected_usd: Option<f64>,
    /// Unpriced sessions excluded from both dollar figures.
    pub unpriced_session_count: i64,
    /// @deprecated Aggregate list-rate cost (actual + projected).
    /// Retained for backward compat; new UI should use the split fields.
    pub spend_usd: Option<f64>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    pub cache_read_tokens: i64,
    /// Shared-credit completed-task count: number of distinct completed tasks
    /// that had at least one worker session using this model.
    pub shared_credit_completed_task_count: i64,
    /// Fraction of closed tasks that completed successfully (0.0–1.0).
    pub success_rate: Option<f64>,
    /// Average total_reopen_count across closed tasks attributed to this model.
    pub avg_reopens: Option<f64>,
    /// Cost per completed task. `None` when no completed tasks or all sessions
    /// were unpriced (NULL cost_usd).
    pub cost_per_completed_task: Option<f64>,
    /// Average total tokens (in + out) per completed task.
    pub tokens_per_task: Option<f64>,
}

/// Project × model matrix entry for frontend consumption.
///
/// Groups usage by project and model across all agent types matching the
/// endpoint filters. NULL-project sessions (chat sessions without a task)
/// are preserved with an empty-string project_id.  Outcome metrics
/// (`success_rate`, `avg_reopens`) are computed over the distinct tasks
/// touched by the cell's sessions.
#[derive(Clone, Debug, Default)]
pub struct ProjectModelMatrixRow {
    /// Project ID, or empty string for sessions without a project.
    pub project_id: String,
    /// Human-readable project name (empty when unknown / no project).
    pub project_name: String,
    pub model_id: String,
    pub sessions: i64,
    /// Real API-key spend in USD.
    pub actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost in USD.
    pub projected_usd: Option<f64>,
    /// Unpriced sessions excluded from both dollar figures.
    pub unpriced_session_count: i64,
    /// @deprecated Aggregate list-rate cost (actual + projected).
    pub spend_usd: Option<f64>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    pub cache_read_tokens: i64,
    /// Distinct tasks touched by this cell's sessions.
    pub task_count: i64,
    /// Fraction of closed tasks that completed successfully (0.0–1.0).
    pub success_rate: Option<f64>,
    /// Average total_reopen_count across closed tasks in this cell.
    pub avg_reopens: Option<f64>,
}

// ── Public types ──────────────────────────────────────────────────────────

/// Grouping dimensions supported by the analytics breakdown query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupDimension {
    Model,
    Project,
    User,
    Proposal,
    Task,
    Agent,
}

impl GroupDimension {
    /// Human-readable label for the group key column in result rows.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Model => "model_id",
            Self::Project => "project_id",
            Self::User => "user_id",
            Self::Proposal => "proposal_id",
            Self::Task => "task_id",
            Self::Agent => "agent_type",
        }
    }

    /// SQL expression yielding the entity id (group key) for breakdowns.
    fn key_expr(&self) -> &'static str {
        match self {
            Self::Model => "s.model_id",
            Self::Project => "COALESCE(s.project_id, '')",
            // User attribution: prefer session creator, fall back to task creator.
            Self::User => "COALESCE(s.created_by_user_id, t.created_by_user_id, '')",
            Self::Proposal => "COALESCE(pe.proposal_id, '')",
            Self::Task => "COALESCE(s.task_id, '')",
            Self::Agent => "s.agent_type",
        }
    }

    /// SQL expression yielding the human-readable display name for breakdowns.
    /// Always wrapped in COALESCE so the column is non-null; the UI falls back
    /// to the id when the name is empty.
    fn name_expr(&self) -> &'static str {
        match self {
            Self::Model => "s.model_id",
            Self::Project => "COALESCE(p.name, '')",
            Self::User => "COALESCE(u.github_name, u.github_login, '')",
            Self::Proposal => "COALESCE(pr.title, '')",
            Self::Task => "COALESCE(t.title, '')",
            Self::Agent => "s.agent_type",
        }
    }

    /// Join clauses required to resolve this dimension's key and name.
    /// The base relation is always `sessions s`.
    fn joins(&self) -> &'static str {
        match self {
            Self::Model | Self::Agent => "LEFT JOIN tasks t ON t.id = s.task_id",
            Self::Project => {
                "LEFT JOIN tasks t ON t.id = s.task_id \
                 LEFT JOIN projects p ON p.id = s.project_id"
            }
            Self::User => {
                "LEFT JOIN tasks t ON t.id = s.task_id \
                 LEFT JOIN users u \
                    ON u.id = COALESCE(s.created_by_user_id, t.created_by_user_id)"
            }
            Self::Task => "LEFT JOIN tasks t ON t.id = s.task_id",
            // Proposal: sessions → tasks → epics → proposal_epics → proposals.
            // INNER JOIN proposal_epics restricts to sessions that trace to a
            // proposal.
            Self::Proposal => {
                "LEFT JOIN tasks t ON t.id = s.task_id \
                 LEFT JOIN epics e ON e.id = t.epic_id \
                 INNER JOIN proposal_epics pe ON pe.epic_id = e.id \
                 LEFT JOIN proposals pr ON pr.id = pe.proposal_id"
            }
        }
    }
}

/// Typed input for usage analytics queries.
#[derive(Clone, Debug)]
pub struct UsageAnalyticsQuery {
    /// Inclusive lower bound (ISO-8601 prefix, e.g. `"2025-01-01"`).
    pub from: String,
    /// Exclusive upper bound.
    pub to: String,
    /// Retained for API compatibility; the dashboard computes all four entity
    /// breakdowns regardless of this value.
    pub group_by: GroupDimension,
    /// Optional project filter.
    pub project_id: Option<String>,
    /// Optional model filter.
    pub model_id: Option<String>,
    /// Optional agent_type filter.
    pub agent_type: Option<String>,
    /// Optional user filter. Matches the session's attributed user, preferring
    /// the session creator and falling back to the task creator (same
    /// attribution rule as the `by_user` breakdown).
    pub user_id: Option<String>,
}

/// Overall totals across the entire queried date range (and matching filters).
#[derive(Clone, Debug, Default)]
pub struct UsageTotals {
    pub session_count: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Real API-key spend in USD (basis = 'actual'). `None` when no actual
    /// sessions are priced.
    pub actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost in USD (basis = 'projected').
    /// `None` when no projected sessions are priced.
    pub projected_usd: Option<f64>,
    /// @deprecated Aggregate list-rate cost across all priced bases.
    /// Retained for backward compatibility; new UI should use the split fields.
    pub total_cost_usd: Option<f64>,
    /// Number of matching sessions with `cost_basis = 'unpriced'`, excluded
    /// from both `actual_spend_usd` and `projected_usd`.
    pub unpriced_session_count: i64,
}

/// A single multi-dimensional time-series row: one day bucket for a particular
/// (model, project, agent_type) combination.  The dashboard sums these client
/// side to render spend grouped by any one of those dimensions.
#[derive(Clone, Debug, Default)]
pub struct SeriesDetailRow {
    /// ISO date prefix, e.g. `"2025-03-14"`.
    pub day: String,
    pub model: String,
    /// Project id, or empty string for sessions without a project.
    pub project_id: String,
    /// Human-readable project name (empty when unknown).
    pub project_name: String,
    pub agent_type: String,
    pub session_count: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    pub cache_read_tokens: i64,
    /// Distinct tasks touched in this bucket.
    pub task_count: i64,
    /// Real API-key spend in USD.
    pub actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost in USD.
    pub projected_usd: Option<f64>,
    /// Unpriced sessions excluded from both dollar figures.
    pub unpriced_session_count: i64,
    /// @deprecated Aggregate list-rate cost (actual + projected).
    pub cost: Option<f64>,
}

/// A single aggregated entity-breakdown row (one user / project / proposal /
/// task), summarised across the whole queried window.
#[derive(Clone, Debug, Default)]
pub struct EntityBreakdownRow {
    /// The entity id (user id, project id, proposal id, task id).
    pub id: String,
    /// Human-readable display name; empty when unknown.
    pub name: String,
    /// Real API-key spend in USD.
    pub actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost in USD.
    pub projected_usd: Option<f64>,
    /// Unpriced sessions excluded from both dollar figures.
    pub unpriced_session_count: i64,
    /// @deprecated Aggregate list-rate cost (actual + projected).
    pub cost: Option<f64>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    pub cache_read_tokens: i64,
    /// Distinct tasks attributed to this entity.
    pub task_count: i64,
    /// Fraction of closed tasks that completed successfully (0.0–1.0).
    pub success_rate: Option<f64>,
    /// Average total_reopen_count across this entity's closed tasks.
    pub avg_reopens: Option<f64>,
}

// ── Repository ────────────────────────────────────────────────────────────

pub struct UsageAnalyticsRepository {
    db: Database,
}

impl UsageAnalyticsRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    // ── Shared filter construction ─────────────────────────────────────────

    /// Build the session-level WHERE conditions (date range + optional
    /// project / model / agent / user filters) and their ordered bind values.
    /// Conditions reference `sessions s` and, for the user filter, `tasks t`;
    /// every caller therefore joins `tasks t ON t.id = s.task_id` (a 1:1 join
    /// that cannot inflate counts).
    fn session_filters(params: &UsageAnalyticsQuery) -> (Vec<String>, Vec<String>) {
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        let mut idx: usize = 1;

        conditions.push(format!("s.started_at >= ${idx}"));
        binds.push(params.from.clone());
        idx += 1;

        conditions.push(format!("s.started_at < ${idx}"));
        binds.push(params.to.clone());
        idx += 1;

        if let Some(ref project_id) = params.project_id {
            conditions.push(format!("s.project_id = ${idx}"));
            binds.push(project_id.clone());
            idx += 1;
        }

        if let Some(ref model_id) = params.model_id {
            conditions.push(format!("s.model_id = ${idx}"));
            binds.push(model_id.clone());
            idx += 1;
        }

        if let Some(ref agent_type) = params.agent_type {
            conditions.push(format!("s.agent_type = ${idx}"));
            binds.push(agent_type.clone());
            idx += 1;
        }

        if let Some(ref user_id) = params.user_id {
            // Same attribution as the `by_user` breakdown: session creator,
            // falling back to the task creator.
            conditions.push(format!(
                "COALESCE(s.created_by_user_id, t.created_by_user_id) = ${idx}"
            ));
            binds.push(user_id.clone());
        }

        (conditions, binds)
    }

    fn where_clause(conditions: &[String]) -> String {
        format!("WHERE {}", conditions.join(" AND "))
    }

    fn bind_all<'q>(
        query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
        binds: &'q [String],
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        let mut q = query;
        for val in binds {
            q = q.bind(val);
        }
        q
    }

    // ── Totals ────────────────────────────────────────────────────────────

    /// Window totals across all matching sessions. The `tasks t` join is 1:1
    /// (`t.id = s.task_id`) so it cannot inflate counts; it exists only so the
    /// optional user filter can fall back to the task creator.
    ///
    /// Cost is split by `cost_basis`:
    ///   - `actual_spend_usd`: `cost_basis = 'actual'` sessions only.
    ///   - `projected_usd`: `cost_basis = 'projected'` sessions only.
    ///   - `unpriced_session_count`: `cost_basis = 'unpriced'` sessions,
    ///     excluded from both dollar figures.
    ///   - `total_cost_usd` (deprecated): sum of all priced sessions.
    pub async fn totals(&self, params: &UsageAnalyticsQuery) -> Result<UsageTotals> {
        self.db.ensure_initialized().await?;

        let (conditions, binds) = Self::session_filters(params);
        let where_clause = Self::where_clause(&conditions);

        let sql = format!(
            "SELECT \
                COUNT(*)                                       AS session_count, \
                COALESCE(SUM(s.tokens_in)::bigint, 0)          AS tokens_in, \
                COALESCE(SUM(s.tokens_out)::bigint, 0)         AS tokens_out, \
                COALESCE(SUM(s.cache_read_tokens)::bigint, 0)  AS cache_read_tokens, \
                COALESCE(SUM(s.cache_write_tokens)::bigint, 0) AS cache_write_tokens, \
                SUM(CASE WHEN s.cost_basis = 'actual' THEN s.cost_usd END)    AS actual_spend_usd, \
                SUM(CASE WHEN s.cost_basis = 'projected' THEN s.cost_usd END) AS projected_usd, \
                SUM(s.cost_usd)                                AS total_cost_usd, \
                COUNT(*) FILTER (WHERE s.cost_basis = 'unpriced') \
                                                               AS unpriced_session_count \
             FROM sessions s \
             LEFT JOIN tasks t ON t.id = s.task_id \
             {where_clause}"
        );

        let query = Self::bind_all(sqlx::query(&sql), &binds);
        let row = query.fetch_one(self.db.pool()).await?;

        Ok(UsageTotals {
            session_count: row.get("session_count"),
            tokens_in: row.get("tokens_in"),
            tokens_out: row.get("tokens_out"),
            cache_read_tokens: row.get("cache_read_tokens"),
            cache_write_tokens: row.get("cache_write_tokens"),
            actual_spend_usd: row.get("actual_spend_usd"),
            projected_usd: row.get("projected_usd"),
            total_cost_usd: row.get("total_cost_usd"),
            unpriced_session_count: row.get("unpriced_session_count"),
        })
    }

    // ── Multi-dimensional time series ──────────────────────────────────────

    /// Daily series carrying model / project / agent dimensions so the UI can
    /// group spend client-side.  One row per (day, model, project, agent).
    pub async fn series_detailed(
        &self,
        params: &UsageAnalyticsQuery,
    ) -> Result<Vec<SeriesDetailRow>> {
        self.db.ensure_initialized().await?;

        let (conditions, binds) = Self::session_filters(params);
        let where_clause = Self::where_clause(&conditions);

        let sql = format!(
            "SELECT \
                substring(s.started_at, 1, 10)                AS day, \
                s.model_id                                    AS model, \
                COALESCE(s.project_id, '')                    AS project_id, \
                COALESCE(p.name, '')                          AS project_name, \
                s.agent_type                                  AS agent_type, \
                COUNT(*)                                       AS session_count, \
                COALESCE(SUM(s.tokens_in)::bigint, 0)          AS tokens_in, \
                COALESCE(SUM(s.tokens_out)::bigint, 0)         AS tokens_out, \
                COALESCE(SUM(s.cache_read_tokens)::bigint, 0)  AS cache_read_tokens, \
                COUNT(DISTINCT s.task_id)                      AS task_count, \
                SUM(CASE WHEN s.cost_basis = 'actual' THEN s.cost_usd END)    AS actual_spend_usd, \
                SUM(CASE WHEN s.cost_basis = 'projected' THEN s.cost_usd END) AS projected_usd, \
                COUNT(*) FILTER (WHERE s.cost_basis = 'unpriced') \
                                                               AS unpriced_session_count, \
                SUM(s.cost_usd)                                AS cost \
             FROM sessions s \
             LEFT JOIN projects p ON p.id = s.project_id \
             LEFT JOIN tasks t ON t.id = s.task_id \
             {where_clause} \
             GROUP BY substring(s.started_at, 1, 10), s.model_id, \
                      COALESCE(s.project_id, ''), COALESCE(p.name, ''), s.agent_type \
             ORDER BY 1"
        );

        let query = Self::bind_all(sqlx::query(&sql), &binds);
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| SeriesDetailRow {
                day: r.get("day"),
                model: r.get("model"),
                project_id: r.get("project_id"),
                project_name: r.get("project_name"),
                agent_type: r.get("agent_type"),
                session_count: r.get("session_count"),
                tokens_in: r.get("tokens_in"),
                tokens_out: r.get("tokens_out"),
                cache_read_tokens: r.get("cache_read_tokens"),
                task_count: r.get("task_count"),
                actual_spend_usd: r.get("actual_spend_usd"),
                projected_usd: r.get("projected_usd"),
                unpriced_session_count: r.get("unpriced_session_count"),
                cost: r.get("cost"),
            })
            .collect())
    }

    // ── Entity breakdown (user / project / proposal / task) ────────────────

    /// Aggregate breakdown for a single entity dimension across the whole
    /// window.  Spend / tokens are session-level sums; `success_rate` and
    /// `avg_reopens` are computed over the entity's distinct tasks so multiple
    /// sessions on one task cannot overweight the outcome metrics.
    pub async fn entity_breakdown(
        &self,
        params: &UsageAnalyticsQuery,
        dimension: GroupDimension,
    ) -> Result<Vec<EntityBreakdownRow>> {
        self.db.ensure_initialized().await?;

        let (conditions, binds) = Self::session_filters(params);
        let where_clause = Self::where_clause(&conditions);
        let key_expr = dimension.key_expr();
        let name_expr = dimension.name_expr();
        let joins = dimension.joins();

        let sql = format!(
            "WITH base AS ( \
                SELECT \
                    {key_expr}  AS entity_id, \
                    {name_expr} AS entity_name, \
                    s.cost_usd, s.cost_basis, s.tokens_in, s.tokens_out, s.cache_read_tokens, \
                    s.task_id, \
                    t.status, t.close_reason, t.total_reopen_count \
                FROM sessions s {joins} {where_clause} \
             ), \
             sess AS ( \
                SELECT \
                    entity_id, \
                    MAX(entity_name) AS entity_name, \
                    SUM(CASE WHEN cost_basis = 'actual' THEN cost_usd END)    AS actual_spend, \
                    SUM(CASE WHEN cost_basis = 'projected' THEN cost_usd END) AS projected, \
                    COUNT(*) FILTER (WHERE cost_basis = 'unpriced')            AS unpriced_count, \
                    SUM(cost_usd) AS cost, \
                    COALESCE(SUM(tokens_in)::bigint, 0)  AS tokens_in, \
                    COALESCE(SUM(tokens_out)::bigint, 0) AS tokens_out, \
                    COALESCE(SUM(cache_read_tokens)::bigint, 0) AS cache_read_tokens, \
                    COUNT(DISTINCT task_id) AS task_count \
                FROM base GROUP BY entity_id \
             ), \
             task_agg AS ( \
                SELECT \
                    entity_id, \
                    COUNT(DISTINCT CASE WHEN status = 'closed' AND close_reason = 'completed' \
                                        THEN task_id END) AS completed_count, \
                    COUNT(DISTINCT CASE WHEN status = 'closed' THEN task_id END) AS closed_count, \
                    AVG(CASE WHEN status = 'closed' \
                             THEN total_reopen_count::DOUBLE PRECISION END) AS avg_reopens \
                FROM ( \
                    SELECT DISTINCT entity_id, task_id, status, close_reason, total_reopen_count \
                    FROM base \
                ) distinct_tasks \
                GROUP BY entity_id \
             ) \
             SELECT \
                 sess.entity_id    AS entity_id, \
                 sess.entity_name  AS entity_name, \
                 sess.actual_spend AS actual_spend_usd, \
                 sess.projected    AS projected_usd, \
                 sess.unpriced_count AS unpriced_session_count, \
                 sess.cost         AS cost, \
                 sess.tokens_in    AS tokens_in, \
                 sess.tokens_out   AS tokens_out, \
                 sess.cache_read_tokens AS cache_read_tokens, \
                 sess.task_count   AS task_count, \
                 CASE WHEN ta.closed_count > 0 \
                      THEN ta.completed_count::DOUBLE PRECISION / ta.closed_count::DOUBLE PRECISION \
                      END          AS success_rate, \
                 ta.avg_reopens    AS avg_reopens \
             FROM sess \
             LEFT JOIN task_agg ta ON ta.entity_id = sess.entity_id \
             WHERE sess.entity_id <> '' \
             ORDER BY sess.cost DESC NULLS LAST"
        );

        let query = Self::bind_all(sqlx::query(&sql), &binds);
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| EntityBreakdownRow {
                id: r.get("entity_id"),
                name: r.get("entity_name"),
                actual_spend_usd: r.get("actual_spend_usd"),
                projected_usd: r.get("projected_usd"),
                unpriced_session_count: r.get("unpriced_session_count"),
                cost: r.get("cost"),
                tokens_in: r.get("tokens_in"),
                tokens_out: r.get("tokens_out"),
                cache_read_tokens: r.get("cache_read_tokens"),
                task_count: r.get("task_count"),
                success_rate: r.get("success_rate"),
                avg_reopens: r.get("avg_reopens"),
            })
            .collect())
    }

    // ── Model effectiveness (worker-scoped) + project × model matrix ───────

    /// Run effectiveness and project-model matrix queries and return the
    /// combined result.  Effectiveness is always scoped to worker sessions;
    /// the matrix obeys all endpoint filters.
    pub async fn query_effectiveness(
        &self,
        params: &UsageAnalyticsQuery,
    ) -> Result<(Vec<ModelEffectivenessRow>, Vec<ProjectModelMatrixRow>)> {
        self.db.ensure_initialized().await?;

        let effectiveness = self.fetch_model_effectiveness(params).await?;
        let matrix = self.fetch_project_model_matrix(params).await?;

        Ok((effectiveness, matrix))
    }

    /// Build FROM + WHERE for the worker-scoped model effectiveness query.
    /// Always scopes to `agent_type = 'worker'`; the endpoint `agent_type`
    /// filter is intentionally ignored because effectiveness is defined over
    /// worker sessions only.
    fn build_effectiveness_from_where(
        params: &UsageAnalyticsQuery,
    ) -> (String, String, Vec<String>) {
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        let mut idx: usize = 1;

        conditions.push(format!("s.started_at >= ${idx}"));
        binds.push(params.from.clone());
        idx += 1;

        conditions.push(format!("s.started_at < ${idx}"));
        binds.push(params.to.clone());
        idx += 1;

        conditions.push(format!("s.agent_type = ${idx}"));
        binds.push("worker".to_string());
        idx += 1;

        if let Some(ref project_id) = params.project_id {
            conditions.push(format!("s.project_id = ${idx}"));
            binds.push(project_id.clone());
            idx += 1;
        }

        if let Some(ref model_id) = params.model_id {
            conditions.push(format!("s.model_id = ${idx}"));
            binds.push(model_id.clone());
            idx += 1;
        }

        if let Some(ref user_id) = params.user_id {
            conditions.push(format!(
                "COALESCE(s.created_by_user_id, t.created_by_user_id) = ${idx}"
            ));
            binds.push(user_id.clone());
        }

        let from_clause = "FROM sessions s JOIN tasks t ON t.id = s.task_id".to_string();
        let where_clause = format!("WHERE {}", conditions.join(" AND "));

        (from_clause, where_clause, binds)
    }

    async fn fetch_model_effectiveness(
        &self,
        params: &UsageAnalyticsQuery,
    ) -> Result<Vec<ModelEffectivenessRow>> {
        let (from_clause, where_clause, binds) = Self::build_effectiveness_from_where(params);

        let sql = format!(
            "WITH filtered_sessions AS ( \
                SELECT \
                    s.model_id, s.task_id, s.cost_usd, s.cost_basis, \
                    s.tokens_in, s.tokens_out, \
                    s.cache_read_tokens, \
                    t.status, t.close_reason, t.total_reopen_count \
                {from_clause} {where_clause} \
             ), \
             session_agg AS ( \
                SELECT \
                    model_id, \
                    COUNT(*) AS sessions, \
                    SUM(CASE WHEN cost_basis = 'actual' THEN cost_usd END)    AS actual_spend_usd, \
                    SUM(CASE WHEN cost_basis = 'projected' THEN cost_usd END) AS projected_usd, \
                    COUNT(*) FILTER (WHERE cost_basis = 'unpriced')            AS unpriced_session_count, \
                    SUM(cost_usd) AS spend_usd, \
                    COALESCE(SUM(tokens_in)::bigint, 0)  AS tokens_in, \
                    COALESCE(SUM(tokens_out)::bigint, 0) AS tokens_out, \
                    COALESCE(SUM(cache_read_tokens)::bigint, 0) AS cache_read_tokens \
                FROM filtered_sessions \
                GROUP BY model_id \
             ), \
             task_agg AS ( \
                SELECT \
                    model_id, \
                    COUNT(DISTINCT CASE WHEN status = 'closed' AND close_reason = 'completed' \
                                        THEN task_id END) AS completed_count, \
                    COUNT(DISTINCT CASE WHEN status = 'closed' THEN task_id END) AS closed_count, \
                    AVG(CASE WHEN status = 'closed' \
                             THEN total_reopen_count::DOUBLE PRECISION END) AS avg_reopens \
                FROM ( \
                    SELECT DISTINCT model_id, task_id, status, close_reason, total_reopen_count \
                    FROM filtered_sessions \
                ) distinct_tasks \
                GROUP BY model_id \
             ) \
             SELECT \
                 sa.model_id                     AS model_id, \
                 sa.sessions                     AS sessions, \
                 sa.actual_spend_usd             AS actual_spend_usd, \
                 sa.projected_usd                AS projected_usd, \
                 sa.unpriced_session_count       AS unpriced_session_count, \
                 sa.spend_usd                    AS spend_usd, \
                 sa.tokens_in                    AS tokens_in, \
                 sa.tokens_out                   AS tokens_out, \
                 sa.cache_read_tokens            AS cache_read_tokens, \
                 COALESCE(ta.completed_count, 0) AS shared_credit_completed_task_count, \
                 CASE WHEN ta.closed_count > 0 \
                      THEN ta.completed_count::DOUBLE PRECISION / ta.closed_count::DOUBLE PRECISION \
                      END                        AS success_rate, \
                 ta.avg_reopens                  AS avg_reopens \
             FROM session_agg sa \
             LEFT JOIN task_agg ta ON ta.model_id = sa.model_id \
             ORDER BY sa.model_id"
        );

        let query = Self::bind_all(sqlx::query(&sql), &binds);
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let sessions: i64 = r.get("sessions");
                let spend_usd: Option<f64> = r.get("spend_usd");
                let tokens_in: i64 = r.get("tokens_in");
                let tokens_out: i64 = r.get("tokens_out");
                let completed: i64 = r.get("shared_credit_completed_task_count");

                let cost_per_completed_task = match (spend_usd, completed) {
                    (Some(cost), c) if c > 0 => Some(cost / c as f64),
                    _ => None,
                };
                let tokens_per_task = if completed > 0 {
                    Some((tokens_in + tokens_out) as f64 / completed as f64)
                } else {
                    None
                };

                ModelEffectivenessRow {
                    model_id: r.get("model_id"),
                    sessions,
                    actual_spend_usd: r.get("actual_spend_usd"),
                    projected_usd: r.get("projected_usd"),
                    unpriced_session_count: r.get("unpriced_session_count"),
                    spend_usd,
                    tokens_in,
                    tokens_out,
                    cache_read_tokens: r.get("cache_read_tokens"),
                    shared_credit_completed_task_count: completed,
                    success_rate: r.get("success_rate"),
                    avg_reopens: r.get("avg_reopens"),
                    cost_per_completed_task,
                    tokens_per_task,
                }
            })
            .collect())
    }

    /// Project × model usage matrix.  Groups all sessions (all agent types) by
    /// project and model, applying all endpoint filters.  NULL-project
    /// sessions are preserved with an empty-string project_id.  Outcome
    /// metrics are computed over the distinct tasks touched by each cell.
    async fn fetch_project_model_matrix(
        &self,
        params: &UsageAnalyticsQuery,
    ) -> Result<Vec<ProjectModelMatrixRow>> {
        let (conditions, binds) = Self::session_filters(params);
        let where_clause = Self::where_clause(&conditions);

        let sql = format!(
            "WITH base AS ( \
                SELECT \
                    COALESCE(s.project_id, '') AS project_id, \
                    COALESCE(p.name, '')       AS project_name, \
                    s.model_id                 AS model_id, \
                    s.cost_usd, s.cost_basis, s.tokens_in, s.tokens_out, s.cache_read_tokens, \
                    s.task_id, \
                    t.status, t.close_reason, t.total_reopen_count \
                FROM sessions s \
                LEFT JOIN projects p ON p.id = s.project_id \
                LEFT JOIN tasks t ON t.id = s.task_id \
                {where_clause} \
             ), \
             sess AS ( \
                SELECT \
                    project_id, MAX(project_name) AS project_name, model_id, \
                    COUNT(*) AS sessions, \
                    SUM(CASE WHEN cost_basis = 'actual' THEN cost_usd END)    AS actual_spend_usd, \
                    SUM(CASE WHEN cost_basis = 'projected' THEN cost_usd END) AS projected_usd, \
                    COUNT(*) FILTER (WHERE cost_basis = 'unpriced')            AS unpriced_session_count, \
                    SUM(cost_usd) AS spend_usd, \
                    COALESCE(SUM(tokens_in)::bigint, 0)  AS tokens_in, \
                    COALESCE(SUM(tokens_out)::bigint, 0) AS tokens_out, \
                    COALESCE(SUM(cache_read_tokens)::bigint, 0) AS cache_read_tokens, \
                    COUNT(DISTINCT task_id) AS task_count \
                FROM base GROUP BY project_id, model_id \
             ), \
             task_agg AS ( \
                SELECT \
                    project_id, model_id, \
                    COUNT(DISTINCT CASE WHEN status = 'closed' AND close_reason = 'completed' \
                                        THEN task_id END) AS completed_count, \
                    COUNT(DISTINCT CASE WHEN status = 'closed' THEN task_id END) AS closed_count, \
                    AVG(CASE WHEN status = 'closed' \
                             THEN total_reopen_count::DOUBLE PRECISION END) AS avg_reopens \
                FROM ( \
                    SELECT DISTINCT project_id, model_id, task_id, status, close_reason, \
                           total_reopen_count \
                    FROM base \
                ) distinct_tasks \
                GROUP BY project_id, model_id \
             ) \
             SELECT \
                 sess.project_id   AS project_id, \
                 sess.project_name AS project_name, \
                 sess.model_id     AS model_id, \
                 sess.sessions     AS sessions, \
                 sess.actual_spend_usd AS actual_spend_usd, \
                 sess.projected_usd    AS projected_usd, \
                 sess.unpriced_session_count AS unpriced_session_count, \
                 sess.spend_usd    AS spend_usd, \
                 sess.tokens_in    AS tokens_in, \
                 sess.tokens_out   AS tokens_out, \
                 sess.cache_read_tokens AS cache_read_tokens, \
                 sess.task_count   AS task_count, \
                 CASE WHEN ta.closed_count > 0 \
                      THEN ta.completed_count::DOUBLE PRECISION / ta.closed_count::DOUBLE PRECISION \
                      END           AS success_rate, \
                 ta.avg_reopens    AS avg_reopens \
             FROM sess \
             LEFT JOIN task_agg ta \
                ON ta.project_id = sess.project_id AND ta.model_id = sess.model_id \
             ORDER BY 1, 3"
        );

        let query = Self::bind_all(sqlx::query(&sql), &binds);
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| ProjectModelMatrixRow {
                project_id: r.get("project_id"),
                project_name: r.get("project_name"),
                model_id: r.get("model_id"),
                sessions: r.get("sessions"),
                actual_spend_usd: r.get("actual_spend_usd"),
                projected_usd: r.get("projected_usd"),
                unpriced_session_count: r.get("unpriced_session_count"),
                spend_usd: r.get("spend_usd"),
                tokens_in: r.get("tokens_in"),
                tokens_out: r.get("tokens_out"),
                cache_read_tokens: r.get("cache_read_tokens"),
                task_count: r.get("task_count"),
                success_rate: r.get("success_rate"),
                avg_reopens: r.get("avg_reopens"),
            })
            .collect())
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_dimension_labels() {
        assert_eq!(GroupDimension::Model.label(), "model_id");
        assert_eq!(GroupDimension::Project.label(), "project_id");
        assert_eq!(GroupDimension::User.label(), "user_id");
        assert_eq!(GroupDimension::Proposal.label(), "proposal_id");
        assert_eq!(GroupDimension::Task.label(), "task_id");
        assert_eq!(GroupDimension::Agent.label(), "agent_type");
    }

    #[test]
    fn user_dimension_key_uses_creator_fallback() {
        let expr = GroupDimension::User.key_expr();
        assert!(expr.contains("COALESCE"));
        assert!(expr.contains("created_by_user_id"));
    }

    #[test]
    fn proposal_dimension_inner_joins_proposal_epics() {
        assert!(
            GroupDimension::Proposal
                .joins()
                .contains("INNER JOIN proposal_epics")
        );
        assert!(GroupDimension::Proposal.name_expr().contains("pr.title"));
    }

    #[test]
    fn breakdown_dimensions_have_non_empty_sql() {
        for dim in [
            GroupDimension::User,
            GroupDimension::Project,
            GroupDimension::Proposal,
            GroupDimension::Task,
        ] {
            assert!(!dim.key_expr().is_empty());
            assert!(!dim.name_expr().is_empty());
            assert!(dim.joins().contains("tasks t"));
        }
    }

    #[test]
    fn session_filters_emit_date_bounds_first() {
        let q = UsageAnalyticsQuery {
            from: "2025-01-01".into(),
            to: "2025-02-01".into(),
            group_by: GroupDimension::Model,
            project_id: Some("proj-1".into()),
            model_id: None,
            agent_type: Some("worker".into()),
            user_id: Some("user-1".into()),
        };
        let (conditions, binds) = UsageAnalyticsRepository::session_filters(&q);
        assert_eq!(conditions[0], "s.started_at >= $1");
        assert_eq!(conditions[1], "s.started_at < $2");
        assert_eq!(binds[0], "2025-01-01");
        assert_eq!(binds[1], "2025-02-01");
        // project_id present, model_id absent, agent_type + user present → 5 binds.
        assert_eq!(binds.len(), 5);
        assert_eq!(binds[2], "proj-1");
        assert_eq!(binds[3], "worker");
        assert_eq!(binds[4], "user-1");
        // The user filter must reference the task-creator fallback so it can
        // bind against the COALESCE attribution column.
        assert!(
            conditions.last().unwrap().contains("created_by_user_id"),
            "user filter should use the COALESCE attribution column"
        );
    }

    #[test]
    fn usage_totals_default() {
        let totals = UsageTotals::default();
        assert_eq!(totals.session_count, 0);
        assert!(totals.total_cost_usd.is_none());
        assert!(totals.actual_spend_usd.is_none());
        assert!(totals.projected_usd.is_none());
        assert_eq!(totals.unpriced_session_count, 0);
    }

    #[test]
    fn series_detail_row_default() {
        let row = SeriesDetailRow::default();
        assert_eq!(row.day, "");
        assert_eq!(row.session_count, 0);
        assert!(row.cost.is_none());
        assert!(row.actual_spend_usd.is_none());
        assert!(row.projected_usd.is_none());
        assert_eq!(row.unpriced_session_count, 0);
    }

    #[test]
    fn entity_breakdown_row_default() {
        let row = EntityBreakdownRow::default();
        assert_eq!(row.id, "");
        assert_eq!(row.task_count, 0);
        assert!(row.cost.is_none());
        assert!(row.actual_spend_usd.is_none());
        assert!(row.projected_usd.is_none());
        assert_eq!(row.unpriced_session_count, 0);
        assert!(row.success_rate.is_none());
    }

    #[test]
    fn project_model_matrix_row_default() {
        let row = ProjectModelMatrixRow::default();
        assert_eq!(row.project_id, "");
        assert_eq!(row.project_name, "");
        assert!(row.spend_usd.is_none());
        assert!(row.actual_spend_usd.is_none());
        assert!(row.projected_usd.is_none());
        assert_eq!(row.unpriced_session_count, 0);
        assert!(row.success_rate.is_none());
    }

    #[test]
    fn model_effectiveness_row_clone_debug() {
        let row = ModelEffectivenessRow {
            model_id: "gpt-4".into(),
            sessions: 5,
            actual_spend_usd: Some(0.8),
            projected_usd: Some(0.43),
            unpriced_session_count: 0,
            spend_usd: Some(1.23),
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 40,
            shared_credit_completed_task_count: 3,
            success_rate: Some(0.67),
            avg_reopens: Some(0.5),
            cost_per_completed_task: Some(0.41),
            tokens_per_task: Some(50.0),
        };
        let row2 = row.clone();
        assert_eq!(row2.model_id, "gpt-4");
        assert_eq!(row2.shared_credit_completed_task_count, 3);
        assert_eq!(row2.unpriced_session_count, 0);
        let _dbg = format!("{row:?}");
    }
}
