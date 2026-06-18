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
            #[allow(unused_assignments)]
            {
                bind_idx += 1;
            }
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
    async fn bind_all<'q>(
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
        let query = Self::bind_all(query, &binds).await;
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
        let query = Self::bind_all(query, &binds).await;
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
        let query = Self::bind_all(query, &binds).await;
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
}
