// ── Usage analytics repository ─────────────────────────────────────────────
//
// Provides raw daily-aggregate and breakdown queries over the `sessions` table
// (plus optional dimension joins) for admin usage analytics.  All SQL uses
// `substring(s.started_at, 1, 10)` for day bucketing — no `date_trunc` or
// `generate_series`.
//
// Cost semantics: `cost_usd` is nullable on the sessions table.  When ANY
// session in an aggregate group has a NULL cost (unpriced model), the group
// aggregate cost is returned as NULL so the UI can render an em-dash instead
// of $0.

use std::fmt;

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
    /// Aggregate cost in USD for worker sessions of this model.
    /// `None` when all sessions used unpriced models (NULL cost_usd).
    pub spend_usd: Option<f64>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Shared-credit completed-task count: number of distinct completed tasks
    /// that had at least one worker session using this model.
    pub shared_credit_completed_task_count: i64,
    /// Fraction of closed tasks that completed successfully (0.0–1.0).
    pub success_rate: Option<f64>,
    /// Average total_reopen_count across closed tasks attributed to this model.
    pub avg_reopens: Option<f64>,
    /// Fraction of closed tasks with zero verification failures (0.0–1.0).
    pub verification_pass_rate: Option<f64>,
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
/// are preserved with an empty-string project_id.
#[derive(Clone, Debug, Default)]
pub struct ProjectModelMatrixRow {
    /// Project ID, or empty string for sessions without a project.
    pub project_id: String,
    pub model_id: String,
    pub sessions: i64,
    /// Aggregate cost in USD. `None` when ANY session in the group had a NULL
    /// cost_usd (unpriced model).
    pub spend_usd: Option<f64>,
    pub tokens_in: i64,
    pub tokens_out: i64,
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
    /// SQL column expression for the group key.
    fn group_expr(&self) -> &'static str {
        match self {
            Self::Model => "s.model_id",
            Self::Project => "COALESCE(s.project_id, '')",
            // User attribution: prefer session creator, fall back to task creator.
            Self::User => "COALESCE(s.created_by_user_id, t.created_by_user_id, '')",
            // Proposal: sessions → tasks → epics → proposal_epics → proposals
            Self::Proposal => "COALESCE(pe.proposal_id, '')",
            Self::Task => "COALESCE(s.task_id, '')",
            Self::Agent => "s.agent_type",
        }
    }

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
}

impl fmt::Display for GroupDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Typed input for usage analytics queries.
#[derive(Clone, Debug)]
pub struct UsageAnalyticsQuery {
    /// Inclusive lower bound (ISO-8601 prefix, e.g. `"2025-01-01"`).
    pub from: String,
    /// Exclusive upper bound.
    pub to: String,
    /// How to group breakdown rows.
    pub group_by: GroupDimension,
    /// Optional project filter.
    pub project_id: Option<String>,
    /// Optional model filter.
    pub model_id: Option<String>,
    /// Optional agent_type filter.
    pub agent_type: Option<String>,
}

/// A single day's aggregate row in the time-series output.
#[derive(Clone, Debug, Default)]
pub struct DailySeriesRow {
    /// ISO date prefix, e.g. `"2025-03-14"`.
    pub day: String,
    pub session_count: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Aggregate cost in USD.  `None` when ANY session in this day had no
    /// pricing (NULL `cost_usd`), preserving the "unpriced" signal.
    pub total_cost_usd: Option<f64>,
}

/// A single row in the breakdown result, keyed by the chosen `GroupDimension`.
#[derive(Clone, Debug, Default)]
pub struct BreakdownRow {
    /// The group key value (model id, user id, proposal id, etc.).
    /// Empty string represents "no value" (e.g. chat sessions without a task).
    pub group_key: String,
    /// ISO date prefix for this row's day bucket.
    pub day: String,
    pub session_count: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Aggregate cost in USD.  NULL-unpriced semantics preserved.
    pub total_cost_usd: Option<f64>,
}

/// Overall totals across the entire queried date range (and matching filters).
#[derive(Clone, Debug, Default)]
pub struct UsageTotals {
    pub session_count: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Aggregate cost in USD.  `None` when ANY matching session had no
    /// pricing, preserving the "unpriced" signal.
    pub total_cost_usd: Option<f64>,
}

/// Aggregated result bundle returned by [`UsageAnalyticsRepository::query`].
#[derive(Clone, Debug, Default)]
pub struct UsageAnalyticsResult {
    pub totals: UsageTotals,
    pub series: Vec<DailySeriesRow>,
    pub breakdown: Vec<BreakdownRow>,
}

// ── Repository ────────────────────────────────────────────────────────────

pub struct UsageAnalyticsRepository {
    db: Database,
}

impl UsageAnalyticsRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Run all three analytics queries (totals, daily series, breakdown)
    /// against the given filters and return the combined result.
    pub async fn query(&self, params: &UsageAnalyticsQuery) -> Result<UsageAnalyticsResult> {
        self.db.ensure_initialized().await?;

        let totals = self.fetch_totals(params).await?;
        let series = self.fetch_daily_series(params).await?;
        let breakdown = self.fetch_breakdown(params).await?;

        Ok(UsageAnalyticsResult {
            totals,
            series,
            breakdown,
        })
    }

    // ── Internal queries ──────────────────────────────────────────────────

    /// Shared FROM + WHERE clause fragment.
    ///
    /// Joins:
    ///   - `LEFT JOIN tasks t ON t.id = s.task_id` (for user attribution and
    ///     optional task/project dimension joins)
    ///   - `LEFT JOIN epics e ON e.id = t.epic_id` (for proposal grouping)
    ///   - `LEFT JOIN proposal_epics pe ON pe.epic_id = e.id` (proposal link)
    ///
    /// Returns `(from_clause, where_clause, binds)` where `binds` is the
    /// ordered list of parameter values.
    fn build_from_where(params: &UsageAnalyticsQuery) -> (String, String, Vec<String>) {
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        let mut bind_idx: usize = 1;

        // Date range filter (always first two binds).
        conditions.push(format!("s.started_at >= ${bind_idx}"));
        binds.push(params.from.clone());
        bind_idx += 1;

        conditions.push(format!("s.started_at < ${bind_idx}"));
        binds.push(params.to.clone());
        bind_idx += 1;

        if let Some(ref project_id) = params.project_id {
            conditions.push(format!("s.project_id = ${bind_idx}"));
            binds.push(project_id.clone());
            bind_idx += 1;
        }

        if let Some(ref model_id) = params.model_id {
            conditions.push(format!("s.model_id = ${bind_idx}"));
            binds.push(model_id.clone());
            bind_idx += 1;
        }

        if let Some(ref agent_type) = params.agent_type {
            conditions.push(format!("s.agent_type = ${bind_idx}"));
            binds.push(agent_type.clone());
        }

        // When grouping by proposal, only include sessions that trace to a
        // proposal (INNER join).  For other dimensions, include all sessions
        // (LEFT join) so chat sessions with no task/project are counted.
        let proposal_join = if params.group_by == GroupDimension::Proposal {
            "INNER JOIN proposal_epics pe ON pe.epic_id = e.id"
        } else {
            "LEFT JOIN proposal_epics pe ON pe.epic_id = e.id"
        };

        let from_clause = format!(
            "FROM sessions s \
             LEFT JOIN tasks t ON t.id = s.task_id \
             LEFT JOIN epics e ON e.id = t.epic_id \
             {proposal_join}"
        );

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        (from_clause, where_clause, binds)
    }

    /// Bind parameters onto a `sqlx::query()` builder.
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

    async fn fetch_totals(&self, params: &UsageAnalyticsQuery) -> Result<UsageTotals> {
        let (from_clause, where_clause, binds) = Self::build_from_where(params);

        let sql = format!(
            "SELECT \
                COUNT(*)                          AS \"session_count!\", \
                COALESCE(SUM(s.tokens_in), 0)     AS \"tokens_in!\", \
                COALESCE(SUM(s.tokens_out), 0)    AS \"tokens_out!\", \
                COALESCE(SUM(s.cache_read_tokens), 0)  AS \"cache_read_tokens!\", \
                COALESCE(SUM(s.cache_write_tokens), 0) AS \"cache_write_tokens!\", \
                CASE \
                    WHEN bool_or(s.cost_usd IS NULL) THEN NULL \
                    ELSE SUM(s.cost_usd) \
                END AS \"total_cost_usd\" \
             {from_clause} {where_clause}"
        );

        let query = sqlx::query(&sql);
        let query = Self::bind_all(query, &binds);
        let row = query.fetch_one(self.db.pool()).await?;

        Ok(UsageTotals {
            session_count: row.get("session_count"),
            tokens_in: row.get("tokens_in"),
            tokens_out: row.get("tokens_out"),
            cache_read_tokens: row.get("cache_read_tokens"),
            cache_write_tokens: row.get("cache_write_tokens"),
            total_cost_usd: row.get("total_cost_usd"),
        })
    }

    // ── Daily series ──────────────────────────────────────────────────────

    async fn fetch_daily_series(
        &self,
        params: &UsageAnalyticsQuery,
    ) -> Result<Vec<DailySeriesRow>> {
        let (from_clause, where_clause, binds) = Self::build_from_where(params);

        let sql = format!(
            "SELECT \
                substring(s.started_at, 1, 10)    AS \"day!\", \
                COUNT(*)                          AS \"session_count!\", \
                COALESCE(SUM(s.tokens_in), 0)     AS \"tokens_in!\", \
                COALESCE(SUM(s.tokens_out), 0)    AS \"tokens_out!\", \
                COALESCE(SUM(s.cache_read_tokens), 0)  AS \"cache_read_tokens!\", \
                COALESCE(SUM(s.cache_write_tokens), 0) AS \"cache_write_tokens!\", \
                CASE \
                    WHEN bool_or(s.cost_usd IS NULL) THEN NULL \
                    ELSE SUM(s.cost_usd) \
                END AS \"total_cost_usd\" \
             {from_clause} {where_clause} \
             GROUP BY substring(s.started_at, 1, 10) \
             ORDER BY day"
        );

        let query = sqlx::query(&sql);
        let query = Self::bind_all(query, &binds);
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| DailySeriesRow {
                day: r.get("day"),
                session_count: r.get("session_count"),
                tokens_in: r.get("tokens_in"),
                tokens_out: r.get("tokens_out"),
                cache_read_tokens: r.get("cache_read_tokens"),
                cache_write_tokens: r.get("cache_write_tokens"),
                total_cost_usd: r.get("total_cost_usd"),
            })
            .collect())
    }

    // ── Breakdown by group dimension ──────────────────────────────────────

    async fn fetch_breakdown(&self, params: &UsageAnalyticsQuery) -> Result<Vec<BreakdownRow>> {
        let (from_clause, where_clause, binds) = Self::build_from_where(params);
        let group_expr = params.group_by.group_expr();

        let sql = format!(
            "SELECT \
                {group_expr}                     AS \"group_key!\", \
                substring(s.started_at, 1, 10)   AS \"day!\", \
                COUNT(*)                         AS \"session_count!\", \
                COALESCE(SUM(s.tokens_in), 0)    AS \"tokens_in!\", \
                COALESCE(SUM(s.tokens_out), 0)   AS \"tokens_out!\", \
                COALESCE(SUM(s.cache_read_tokens), 0)  AS \"cache_read_tokens!\", \
                COALESCE(SUM(s.cache_write_tokens), 0) AS \"cache_write_tokens!\", \
                CASE \
                    WHEN bool_or(s.cost_usd IS NULL) THEN NULL \
                    ELSE SUM(s.cost_usd) \
                END AS \"total_cost_usd\" \
             {from_clause} {where_clause} \
             GROUP BY {group_expr}, substring(s.started_at, 1, 10) \
             ORDER BY day, group_key"
        );

        let query = sqlx::query(&sql);
        let query = Self::bind_all(query, &binds);
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| BreakdownRow {
                group_key: r.get("group_key"),
                day: r.get("day"),
                session_count: r.get("session_count"),
                tokens_in: r.get("tokens_in"),
                tokens_out: r.get("tokens_out"),
                cache_read_tokens: r.get("cache_read_tokens"),
                cache_write_tokens: r.get("cache_write_tokens"),
                total_cost_usd: r.get("total_cost_usd"),
            })
            .collect())
    }

    // ── Model effectiveness (worker-scoped) ──────────────────────────────

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

    /// Build FROM + WHERE for the model effectiveness query.
    ///
    /// Always scopes to `agent_type = 'worker'`.  Applies date range,
    /// `project_id`, and `model_id` endpoint filters; the `agent_type`
    /// filter from the endpoint query is intentionally ignored here because
    /// effectiveness is defined over worker sessions only.
    fn build_effectiveness_from_where(
        params: &UsageAnalyticsQuery,
    ) -> (String, String, Vec<String>) {
        let mut conditions: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        let mut bind_idx: usize = 1;

        // Date range (same as base query).
        conditions.push(format!("s.started_at >= ${bind_idx}"));
        binds.push(params.from.clone());
        bind_idx += 1;

        conditions.push(format!("s.started_at < ${bind_idx}"));
        binds.push(params.to.clone());
        bind_idx += 1;

        // Always worker-scoped.
        conditions.push(format!("s.agent_type = ${bind_idx}"));
        binds.push("worker".to_string());
        bind_idx += 1;

        if let Some(ref project_id) = params.project_id {
            conditions.push(format!("s.project_id = ${bind_idx}"));
            binds.push(project_id.clone());
            bind_idx += 1;
        }

        if let Some(ref model_id) = params.model_id {
            conditions.push(format!("s.model_id = ${bind_idx}"));
            binds.push(model_id.clone());
            // bind_idx not incremented — last use
        }

        let from_clause = "FROM sessions s \
             JOIN tasks t ON t.id = s.task_id"
            .to_string();

        let where_clause = format!("WHERE {}", conditions.join(" AND "));

        (from_clause, where_clause, binds)
    }

    /// Per-model effectiveness metrics over worker sessions.
    ///
    /// Uses shared-credit attribution: each completed task counts for every
    /// model that ran at least one worker session on it.  Success rate,
    /// average reopens, and verification pass rate reuse the same pattern as
    /// `AgentRepository::get_metrics`.
    async fn fetch_model_effectiveness(
        &self,
        params: &UsageAnalyticsQuery,
    ) -> Result<Vec<ModelEffectivenessRow>> {
        let (from_clause, where_clause, binds) = Self::build_effectiveness_from_where(params);

        let sql = format!(
            "SELECT \
                s.model_id                               AS \"model_id!\", \
                COUNT(*)                                 AS \"sessions!\", \
                CASE \
                    WHEN bool_or(s.cost_usd IS NULL) THEN NULL \
                    ELSE SUM(s.cost_usd) \
                END                                      AS \"spend_usd\", \
                COALESCE(SUM(s.tokens_in), 0)            AS \"tokens_in!\", \
                COALESCE(SUM(s.tokens_out), 0)           AS \"tokens_out!\", \
                COUNT(DISTINCT \
                    CASE WHEN t.status = 'closed' \
                              AND t.close_reason = 'completed' \
                         THEN t.id END \
                )                                        AS \"shared_credit_completed_task_count!\", \
                CAST(SUM(CASE WHEN t.status = 'closed' AND t.close_reason = 'completed' THEN 1 ELSE 0 END) AS DOUBLE PRECISION) \
                    / CAST(GREATEST(1, COUNT(DISTINCT \
                        CASE WHEN t.status = 'closed' THEN t.id END \
                    )) AS DOUBLE PRECISION)              AS \"success_rate: f64\", \
                COALESCE(AVG(CASE WHEN t.status = 'closed' \
                    THEN CAST(t.total_reopen_count AS DOUBLE PRECISION) \
                    ELSE NULL END), 0.0)                 AS \"avg_reopens!: f64\", \
                CAST(SUM(CASE WHEN t.status = 'closed' AND t.total_verification_failure_count = 0 THEN 1 ELSE 0 END) AS DOUBLE PRECISION) \
                    / CAST(GREATEST(1, COUNT(DISTINCT \
                        CASE WHEN t.status = 'closed' THEN t.id END \
                    )) AS DOUBLE PRECISION)              AS \"verification_pass_rate: f64\" \
             {from_clause} {where_clause} \
             GROUP BY s.model_id \
             ORDER BY s.model_id"
        );

        let mut query = sqlx::query(&sql);
        for val in &binds {
            query = query.bind(val.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let sessions: i64 = r.get("sessions");
                let spend_usd: Option<f64> = r.get("spend_usd");
                let tokens_in: i64 = r.get("tokens_in");
                let tokens_out: i64 = r.get("tokens_out");
                let completed: i64 = r.get("shared_credit_completed_task_count");

                // cost_per_completed_task: NULL when no completed tasks or
                // all sessions were unpriced (NULL cost_usd).
                let cost_per_completed_task = match (spend_usd, completed) {
                    (Some(cost), c) if c > 0 => Some(cost / c as f64),
                    _ => None,
                };
                // tokens_per_task: NULL when no completed tasks.
                let tokens_per_task = if completed > 0 {
                    Some((tokens_in + tokens_out) as f64 / completed as f64)
                } else {
                    None
                };

                ModelEffectivenessRow {
                    model_id: r.get("model_id"),
                    sessions,
                    spend_usd,
                    tokens_in,
                    tokens_out,
                    shared_credit_completed_task_count: completed,
                    success_rate: r.get("success_rate"),
                    avg_reopens: r.get("avg_reopens"),
                    verification_pass_rate: r.get("verification_pass_rate"),
                    cost_per_completed_task,
                    tokens_per_task,
                }
            })
            .collect())
    }

    // ── Project × model matrix ───────────────────────────────────────────

    /// Project × model usage matrix.  Groups all sessions (all agent types)
    /// by project and model, applying all endpoint filters.  NULL-project
    /// sessions are preserved with an empty-string project_id.
    async fn fetch_project_model_matrix(
        &self,
        params: &UsageAnalyticsQuery,
    ) -> Result<Vec<ProjectModelMatrixRow>> {
        let (from_clause, where_clause, binds) = Self::build_from_where(params);

        let sql = format!(
            "SELECT \
                COALESCE(s.project_id, '')               AS \"project_id!\", \
                s.model_id                               AS \"model_id!\", \
                COUNT(*)                                 AS \"sessions!\", \
                CASE \
                    WHEN bool_or(s.cost_usd IS NULL) THEN NULL \
                    ELSE SUM(s.cost_usd) \
                END                                      AS \"spend_usd\", \
                COALESCE(SUM(s.tokens_in), 0)            AS \"tokens_in!\", \
                COALESCE(SUM(s.tokens_out), 0)           AS \"tokens_out!\" \
             {from_clause} {where_clause} \
             GROUP BY COALESCE(s.project_id, ''), s.model_id \
             ORDER BY project_id, model_id"
        );

        let query = sqlx::query(&sql);
        let query = Self::bind_all(query, &binds);
        let rows = query.fetch_all(self.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|r| ProjectModelMatrixRow {
                project_id: r.get("project_id"),
                model_id: r.get("model_id"),
                sessions: r.get("sessions"),
                spend_usd: r.get("spend_usd"),
                tokens_in: r.get("tokens_in"),
                tokens_out: r.get("tokens_out"),
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
    fn group_dimension_display_matches_label() {
        for dim in [
            GroupDimension::Model,
            GroupDimension::Project,
            GroupDimension::User,
            GroupDimension::Proposal,
            GroupDimension::Task,
            GroupDimension::Agent,
        ] {
            assert_eq!(dim.to_string(), dim.label());
        }
    }

    #[test]
    fn group_dimension_sql_expressions() {
        // Verify SQL expressions are reasonable and non-empty.
        assert!(!GroupDimension::Model.group_expr().is_empty());
        assert!(!GroupDimension::Project.group_expr().is_empty());
        assert!(!GroupDimension::User.group_expr().is_empty());
        assert!(!GroupDimension::Proposal.group_expr().is_empty());
        assert!(!GroupDimension::Task.group_expr().is_empty());
        assert!(!GroupDimension::Agent.group_expr().is_empty());
    }

    #[test]
    fn user_group_uses_coalesce() {
        let expr = GroupDimension::User.group_expr();
        assert!(expr.contains("COALESCE"));
        assert!(expr.contains("created_by_user_id"));
    }

    #[test]
    fn proposal_group_joins_through_epics() {
        let expr = GroupDimension::Proposal.group_expr();
        assert!(expr.contains("proposal_id"));
    }

    #[test]
    fn usage_analytics_query_clone_debug() {
        let q = UsageAnalyticsQuery {
            from: "2025-01-01".into(),
            to: "2025-02-01".into(),
            group_by: GroupDimension::Model,
            project_id: Some("proj-1".into()),
            model_id: None,
            agent_type: Some("worker".into()),
        };
        let q2 = q.clone();
        assert_eq!(q2.from, "2025-01-01");
        assert_eq!(q2.to, "2025-02-01");
        assert_eq!(q2.group_by, GroupDimension::Model);
        assert_eq!(q2.project_id.as_deref(), Some("proj-1"));
        assert!(q2.model_id.is_none());
        assert_eq!(q2.agent_type.as_deref(), Some("worker"));

        // Verify Debug is derived (doesn't panic).
        let _dbg = format!("{q:?}");
    }

    #[test]
    fn daily_series_row_default() {
        let row = DailySeriesRow::default();
        assert_eq!(row.day, "");
        assert_eq!(row.session_count, 0);
        assert_eq!(row.tokens_in, 0);
        assert_eq!(row.tokens_out, 0);
        assert_eq!(row.cache_read_tokens, 0);
        assert_eq!(row.cache_write_tokens, 0);
        assert!(row.total_cost_usd.is_none());
    }

    #[test]
    fn breakdown_row_default() {
        let row = BreakdownRow::default();
        assert_eq!(row.group_key, "");
        assert_eq!(row.day, "");
        assert!(row.total_cost_usd.is_none());
    }

    #[test]
    fn usage_totals_default() {
        let totals = UsageTotals::default();
        assert_eq!(totals.session_count, 0);
        assert_eq!(totals.tokens_in, 0);
        assert_eq!(totals.tokens_out, 0);
        assert_eq!(totals.cache_read_tokens, 0);
        assert_eq!(totals.cache_write_tokens, 0);
        assert!(totals.total_cost_usd.is_none());
    }

    #[test]
    fn usage_analytics_result_default() {
        let result = UsageAnalyticsResult::default();
        assert_eq!(result.totals.session_count, 0);
        assert!(result.series.is_empty());
        assert!(result.breakdown.is_empty());
    }

    #[test]
    fn model_effectiveness_row_default() {
        let row = ModelEffectivenessRow::default();
        assert_eq!(row.model_id, "");
        assert_eq!(row.sessions, 0);
        assert!(row.spend_usd.is_none());
        assert_eq!(row.tokens_in, 0);
        assert_eq!(row.tokens_out, 0);
        assert_eq!(row.shared_credit_completed_task_count, 0);
        assert!(row.success_rate.is_none());
        assert!(row.avg_reopens.is_none());
        assert!(row.verification_pass_rate.is_none());
        assert!(row.cost_per_completed_task.is_none());
        assert!(row.tokens_per_task.is_none());
    }

    #[test]
    fn project_model_matrix_row_default() {
        let row = ProjectModelMatrixRow::default();
        assert_eq!(row.project_id, "");
        assert_eq!(row.model_id, "");
        assert_eq!(row.sessions, 0);
        assert!(row.spend_usd.is_none());
        assert_eq!(row.tokens_in, 0);
        assert_eq!(row.tokens_out, 0);
    }

    #[test]
    fn model_effectiveness_row_clone_debug() {
        let row = ModelEffectivenessRow {
            model_id: "gpt-4".into(),
            sessions: 5,
            spend_usd: Some(1.23),
            tokens_in: 100,
            tokens_out: 50,
            shared_credit_completed_task_count: 3,
            success_rate: Some(0.67),
            avg_reopens: Some(0.5),
            verification_pass_rate: Some(0.8),
            cost_per_completed_task: Some(0.41),
            tokens_per_task: Some(50.0),
        };
        let row2 = row.clone();
        assert_eq!(row2.model_id, "gpt-4");
        assert_eq!(row2.shared_credit_completed_task_count, 3);
        // Debug doesn't panic.
        let _dbg = format!("{row:?}");
    }
}

#[cfg(test)]
mod usage_analytics_regression_tests {
    use super::*;
    use crate::Database;

    async fn seed_project(db: &Database, name: &str) -> String {
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind(name)
        .bind("analytics-tests")
        .bind(format!("{name}-{id}"))
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn seed_user(db: &Database, login: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let github_id = i64::from_be_bytes(uuid::Uuid::now_v7().as_bytes()[8..16].try_into().unwrap())
            .unsigned_abs() as i64;
        sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
            .bind(&id)
            .bind(github_id)
            .bind(login)
            .execute(db.pool())
            .await
            .unwrap();
        id
    }

    async fn seed_epic(db: &Database, project_id: &str, title: &str) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = &id[..4];
        sqlx::query(
            "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs) \
             VALUES ($1, $2, $3, $4, 'analytics epic', '📊', 'blue', 'analytics', '[]'::jsonb)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(short_id)
        .bind(title)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    async fn seed_task(
        db: &Database,
        project_id: &str,
        epic_id: Option<&str>,
        status: &str,
        close_reason: Option<&str>,
        created_by_user_id: Option<&str>,
    ) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        let short_id = &id[..4];
        sqlx::query(
            "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, issue_type, \
                 status, priority, owner, labels, acceptance_criteria, memory_refs, close_reason, \
                 created_by_user_id, total_reopen_count, total_verification_failure_count) \
             VALUES ($1, $2, $3, $4, 'analytics task', 'desc', 'design', 'task', $5, 1, 'analytics', \
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $6, $7, 2, 0)",
        )
        .bind(&id)
        .bind(project_id)
        .bind(short_id)
        .bind(epic_id)
        .bind(status)
        .bind(close_reason)
        .bind(created_by_user_id)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    struct SessionSeed<'a> {
        project_id: Option<&'a str>,
        task_id: Option<&'a str>,
        model_id: &'a str,
        agent_type: &'a str,
        started_at: &'a str,
        tokens_in: i64,
        tokens_out: i64,
        cost_usd: Option<f64>,
        created_by_user_id: Option<&'a str>,
    }

    async fn seed_session(db: &Database, seed: SessionSeed<'_>) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, started_at, status, \
                 tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd, created_by_user_id) \
             VALUES ($1, $2, $3, $4, $5, $6, 'completed', $7, $8, 0, 0, $9, $10)",
        )
        .bind(&id)
        .bind(seed.project_id)
        .bind(seed.task_id)
        .bind(seed.model_id)
        .bind(seed.agent_type)
        .bind(seed.started_at)
        .bind(seed.tokens_in)
        .bind(seed.tokens_out)
        .bind(seed.cost_usd)
        .bind(seed.created_by_user_id)
        .execute(db.pool())
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn group_by_user_coalesces_session_creator_before_task_creator() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db, "usage-user-coalesce").await;
        let task_creator = seed_user(&db, "task-creator").await;
        let session_creator = seed_user(&db, "session-creator").await;
        let task_id = seed_task(&db, &project_id, None, "open", None, Some(&task_creator)).await;

        seed_session(&db, SessionSeed {
            project_id: Some(&project_id), task_id: Some(&task_id), model_id: "model-a", agent_type: "worker",
            started_at: "2025-04-01T00:00:00.000Z", tokens_in: 10, tokens_out: 5, cost_usd: Some(0.0), created_by_user_id: None,
        }).await;
        seed_session(&db, SessionSeed {
            project_id: Some(&project_id), task_id: Some(&task_id), model_id: "model-a", agent_type: "worker",
            started_at: "2025-04-02T00:00:00.000Z", tokens_in: 20, tokens_out: 5, cost_usd: Some(1.0), created_by_user_id: Some(&session_creator),
        }).await;

        let result = UsageAnalyticsRepository::new(db)
            .query(&UsageAnalyticsQuery { from: "2025-04-01".into(), to: "2025-04-03".into(), group_by: GroupDimension::User, project_id: Some(project_id), model_id: None, agent_type: None })
            .await.unwrap();

        assert_eq!(result.breakdown.len(), 2);
        assert!(result.breakdown.iter().any(|r| r.group_key == task_creator && r.tokens_in == 10));
        assert!(result.breakdown.iter().any(|r| r.group_key == session_creator && r.tokens_in == 20));
    }

    #[tokio::test]
    async fn group_by_proposal_follows_proposal_epics_linkage() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db, "usage-proposal-link").await;
        let epic_id = seed_epic(&db, &project_id, "proposal-linked").await;
        let task_id = seed_task(&db, &project_id, Some(&epic_id), "open", None, None).await;
        let proposal_id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO proposals (id, short_id, title, body) VALUES ($1, $2, 'Analytics proposal', 'body')")
            .bind(&proposal_id).bind(&proposal_id[..4]).execute(db.pool()).await.unwrap();
        sqlx::query("INSERT INTO proposal_epics (proposal_id, epic_id, project_id) VALUES ($1, $2, $3)")
            .bind(&proposal_id).bind(&epic_id).bind(&project_id).execute(db.pool()).await.unwrap();
        seed_session(&db, SessionSeed {
            project_id: Some(&project_id), task_id: Some(&task_id), model_id: "proposal-model", agent_type: "worker",
            started_at: "2025-05-01T00:00:00.000Z", tokens_in: 11, tokens_out: 7, cost_usd: Some(2.5), created_by_user_id: None,
        }).await;

        let result = UsageAnalyticsRepository::new(db)
            .query(&UsageAnalyticsQuery { from: "2025-05-01".into(), to: "2025-05-02".into(), group_by: GroupDimension::Proposal, project_id: Some(project_id), model_id: None, agent_type: None })
            .await.unwrap();

        assert_eq!(result.breakdown.len(), 1);
        assert_eq!(result.breakdown[0].group_key, proposal_id);
        assert_eq!(result.breakdown[0].tokens_in, 11);
    }

    #[tokio::test]
    async fn effectiveness_is_worker_only_and_uses_shared_completed_task_credit() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db, "usage-effectiveness").await;
        let task_id = seed_task(&db, &project_id, None, "closed", Some("completed"), None).await;
        for (model_id, agent_type) in [
            ("worker-model-a", "worker"),
            ("worker-model-a", "planner"),
            ("worker-model-b", "worker"),
            ("planner-model", "planner"),
            ("reviewer-model", "reviewer"),
            ("chat-model", "chat"),
        ] {
            seed_session(&db, SessionSeed {
                project_id: if agent_type == "chat" { None } else { Some(&project_id) },
                task_id: if agent_type == "chat" { None } else { Some(&task_id) },
                model_id, agent_type, started_at: "2025-06-01T00:00:00.000Z", tokens_in: 10, tokens_out: 5, cost_usd: Some(0.25), created_by_user_id: None,
            }).await;
        }

        let (effectiveness, _) = UsageAnalyticsRepository::new(db)
            .query_effectiveness(&UsageAnalyticsQuery { from: "2025-06-01".into(), to: "2025-06-02".into(), group_by: GroupDimension::Model, project_id: None, model_id: None, agent_type: None })
            .await.unwrap();

        let models: Vec<_> = effectiveness.iter().map(|r| r.model_id.as_str()).collect();
        assert_eq!(models, vec!["worker-model-a", "worker-model-b"]);
        assert!(effectiveness.iter().all(|r| r.sessions == 1));
        assert!(effectiveness
            .iter()
            .all(|r| r.shared_credit_completed_task_count == 1));
        assert!(effectiveness.iter().all(|r| r.success_rate == Some(1.0)));
    }

    #[tokio::test]
    async fn null_cost_semantics_distinguish_unpriced_from_zero_spend() {
        let db = Database::open_in_memory().unwrap();
        let project_id = seed_project(&db, "usage-null-cost").await;
        let zero_task = seed_task(&db, &project_id, None, "closed", Some("completed"), None).await;
        let null_task = seed_task(&db, &project_id, None, "closed", Some("completed"), None).await;
        seed_session(&db, SessionSeed { project_id: Some(&project_id), task_id: Some(&zero_task), model_id: "priced-zero", agent_type: "worker", started_at: "2025-07-01T00:00:00.000Z", tokens_in: 10, tokens_out: 10, cost_usd: Some(0.0), created_by_user_id: None }).await;
        seed_session(&db, SessionSeed { project_id: Some(&project_id), task_id: Some(&null_task), model_id: "unpriced", agent_type: "worker", started_at: "2025-07-01T01:00:00.000Z", tokens_in: 20, tokens_out: 20, cost_usd: None, created_by_user_id: None }).await;

        let repo = UsageAnalyticsRepository::new(db);
        let query = UsageAnalyticsQuery { from: "2025-07-01".into(), to: "2025-07-02".into(), group_by: GroupDimension::Model, project_id: Some(project_id), model_id: None, agent_type: None };
        let result = repo.query(&query).await.unwrap();
        let (effectiveness, matrix) = repo.query_effectiveness(&query).await.unwrap();

        assert!(result.totals.total_cost_usd.is_none(), "mixed priced/unpriced totals stay null");
        let zero_breakdown = result.breakdown.iter().find(|r| r.group_key == "priced-zero").unwrap();
        let unpriced_breakdown = result.breakdown.iter().find(|r| r.group_key == "unpriced").unwrap();
        assert_eq!(zero_breakdown.total_cost_usd, Some(0.0));
        assert!(unpriced_breakdown.total_cost_usd.is_none());
        let zero_eff = effectiveness.iter().find(|r| r.model_id == "priced-zero").unwrap();
        let null_eff = effectiveness.iter().find(|r| r.model_id == "unpriced").unwrap();
        assert_eq!(zero_eff.spend_usd, Some(0.0));
        assert_eq!(zero_eff.cost_per_completed_task, Some(0.0));
        assert!(null_eff.spend_usd.is_none());
        assert!(null_eff.cost_per_completed_task.is_none());
        let zero_matrix = matrix.iter().find(|r| r.model_id == "priced-zero").unwrap();
        let null_matrix = matrix.iter().find(|r| r.model_id == "unpriced").unwrap();
        assert_eq!(zero_matrix.spend_usd, Some(0.0));
        assert!(null_matrix.spend_usd.is_none());
    }
}
