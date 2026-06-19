// HTTP handler for the `/api/admin/usage` REST endpoint consumed by the
// admin analytics UI.
//
// Returns the proposal response shape for usage/cost reporting:
// `totals`, `previous_totals`, `series`, `breakdown`, `model_effectiveness`,
// and `project_model_matrix`. The repository deliberately returns daily rows;
// this handler maps those ISO day buckets into requested day/week/month periods
// using deterministic Rust date arithmetic.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::auth::require_admin;
use djinn_db::{
    BreakdownRow, DailySeriesRow, GroupDimension, ModelEffectivenessRow, ProjectModelMatrixRow,
    UsageAnalyticsQuery, UsageAnalyticsRepository, UsageAnalyticsResult, UsageTotals,
};

pub(super) fn router() -> Router<AppState> {
    // Namespaced under `/api/admin` so it cannot shadow the SPA client-side
    // route of the same name: a hard refresh on `/admin/usage` must fall
    // through to the static `index.html` fallback, not hit this JSON handler.
    Router::new().route("/api/admin/usage", get(usage_handler))
}

// ── Query parsing ────────────────────────────────────────────────────────────

/// Raw query params deserialised from the request URL.  All are optional;
/// defaults and validation are applied in [`UsageQuery::into_typed`].
#[derive(Debug, Deserialize)]
struct UsageQuery {
    from: Option<String>,
    to: Option<String>,
    granularity: Option<String>,
    group_by: Option<String>,
    project_id: Option<String>,
    model_id: Option<String>,
    agent_type: Option<String>,
}

/// Time-series bucket granularity. Repository rows are daily; week/month
/// variants are rolled up in this handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Granularity {
    Day,
    Week,
    Month,
}

impl Granularity {
    fn parse(raw: Option<&str>) -> Result<Self, (StatusCode, String)> {
        match raw.map(str::to_ascii_lowercase).as_deref() {
            None | Some("day") => Ok(Self::Day),
            Some("week") => Ok(Self::Week),
            Some("month") => Ok(Self::Month),
            Some(other) => Err((
                StatusCode::BAD_REQUEST,
                format!("invalid granularity '{other}' (expected day, week, or month)"),
            )),
        }
    }
}

/// Parse a `group_by` query value into the repository's `GroupDimension`.
/// Accepts the same identifiers used in the SQL column expressions.
fn parse_group_by(raw: Option<&str>) -> Result<GroupDimension, (StatusCode, String)> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None | Some("model") => Ok(GroupDimension::Model),
        Some("project") => Ok(GroupDimension::Project),
        Some("user") => Ok(GroupDimension::User),
        Some("proposal") => Ok(GroupDimension::Proposal),
        Some("task") => Ok(GroupDimension::Task),
        Some("agent") => Ok(GroupDimension::Agent),
        Some(other) => Err((
            StatusCode::BAD_REQUEST,
            format!(
                "invalid group_by '{other}' (expected model, project, user, proposal, task, or agent)"
            ),
        )),
    }
}

/// Default `from`/`to` window (last 30 days, ISO-8601) when the client omits
/// either bound.  Kept here rather than the repository so the endpoint owns
/// its own HTTP-facing defaults.
fn default_window() -> (String, String) {
    use time::format_description::well_known::Rfc3339;
    let now = time::OffsetDateTime::now_utc();
    let start = now - time::Duration::days(30);
    let to = now.format(&Rfc3339).unwrap_or_default();
    let from = start.format(&Rfc3339).unwrap_or_default();
    (from, to)
}

/// Validate an HTTP date bound while preserving the original string passed to
/// the repository. The DB query compares ISO-8601 strings, so callers may pass
/// either a date (`YYYY-MM-DD`) or a timestamp whose first 10 characters are an
/// ISO date prefix.
fn parse_iso_date_prefix(name: &str, value: &str) -> Result<time::Date, (StatusCode, String)> {
    let Some(date) = value.get(..10) else {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        ));
    };

    let bytes = date.as_bytes();
    let valid_shape = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, b)| idx == 4 || idx == 7 || b.is_ascii_digit());
    if !valid_shape {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        ));
    }

    let year = date[0..4].parse::<i32>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        )
    })?;
    let month = date[5..7].parse::<u8>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        )
    })?;
    let day = date[8..10].parse::<u8>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        )
    })?;
    let month = time::Month::try_from(month).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        )
    })?;
    time::Date::from_calendar_date(year, month, day).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        )
    })
}

fn validate_date_bound(name: &str, value: String) -> Result<String, (StatusCode, String)> {
    parse_iso_date_prefix(name, &value)?;

    Ok(value)
}

fn validate_date_range(from: String, to: String) -> Result<(String, String), (StatusCode, String)> {
    let from = validate_date_bound("from", from)?;
    let to = validate_date_bound("to", to)?;
    let from_date = parse_iso_date_prefix("from", &from)?;
    let to_date = parse_iso_date_prefix("to", &to)?;
    if from_date >= to_date {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid date range: from must be before to".to_string(),
        ));
    }
    Ok((from, to))
}

fn format_date(date: time::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn previous_window_query(
    query: &UsageAnalyticsQuery,
) -> Result<UsageAnalyticsQuery, (StatusCode, String)> {
    let from = parse_iso_date_prefix("from", &query.from)?;
    let to = parse_iso_date_prefix("to", &query.to)?;
    let span_days = to.to_julian_day() - from.to_julian_day();
    if span_days <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid date range: from must be before to".to_string(),
        ));
    }

    let previous_from =
        time::Date::from_julian_day(from.to_julian_day() - span_days).map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "invalid date range: previous window is out of range".to_string(),
            )
        })?;

    Ok(UsageAnalyticsQuery {
        from: format_date(previous_from),
        to: format_date(from),
        group_by: query.group_by,
        project_id: query.project_id.clone(),
        model_id: query.model_id.clone(),
        agent_type: query.agent_type.clone(),
    })
}

impl UsageQuery {
    /// Validate the raw params and produce the typed repository query plus the
    /// parsed granularity.  Returns an HTTP-style error tuple on bad input.
    fn into_typed(self) -> Result<(UsageAnalyticsQuery, Granularity), (StatusCode, String)> {
        let granularity = Granularity::parse(self.granularity.as_deref())?;
        let group_by = parse_group_by(self.group_by.as_deref())?;

        let (default_from, default_to) = default_window();
        let from = self.from.unwrap_or(default_from);
        let to = self.to.unwrap_or(default_to);
        let (from, to) = validate_date_range(from, to)?;

        Ok((
            UsageAnalyticsQuery {
                from,
                to,
                group_by,
                project_id: self.project_id.filter(|s| !s.is_empty()),
                model_id: self.model_id.filter(|s| !s.is_empty()),
                agent_type: self.agent_type.filter(|s| !s.is_empty()),
            },
            granularity,
        ))
    }
}

// ── Response DTOs ────────────────────────────────────────────────────────────
//
// Cost fields are `Option<f64>` to preserve the repository's NULL/unpriced
// semantics: an aggregate group with any unpriced model surfaces `null` in
// JSON so the UI can render an em-dash instead of `$0`.

#[derive(Serialize)]
struct TotalsDto {
    session_count: i64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: Option<f64>,
}

impl From<UsageTotals> for TotalsDto {
    fn from(t: UsageTotals) -> Self {
        Self {
            session_count: t.session_count,
            tokens_in: t.tokens_in,
            tokens_out: t.tokens_out,
            cache_read_tokens: t.cache_read_tokens,
            cache_write_tokens: t.cache_write_tokens,
            total_cost_usd: t.total_cost_usd,
        }
    }
}

#[derive(Serialize)]
struct SeriesPointDto {
    day: String,
    session_count: i64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: Option<f64>,
}

impl From<DailySeriesRow> for SeriesPointDto {
    fn from(r: DailySeriesRow) -> Self {
        Self {
            day: r.day,
            session_count: r.session_count,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            total_cost_usd: r.total_cost_usd,
        }
    }
}

#[derive(Serialize)]
struct BreakdownPointDto {
    group_key: String,
    day: String,
    session_count: i64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: Option<f64>,
}

impl From<BreakdownRow> for BreakdownPointDto {
    fn from(r: BreakdownRow) -> Self {
        Self {
            group_key: r.group_key,
            day: r.day,
            session_count: r.session_count,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            total_cost_usd: r.total_cost_usd,
        }
    }
}

#[derive(Debug, Default)]
struct UsageAccumulator {
    session_count: i64,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    total_cost_usd: f64,
    cost_known: bool,
}

impl UsageAccumulator {
    fn add_values(
        &mut self,
        session_count: i64,
        tokens_in: i64,
        tokens_out: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
        total_cost_usd: Option<f64>,
    ) {
        self.session_count += session_count;
        self.tokens_in += tokens_in;
        self.tokens_out += tokens_out;
        self.cache_read_tokens += cache_read_tokens;
        self.cache_write_tokens += cache_write_tokens;

        let has_usage = session_count != 0
            || tokens_in != 0
            || tokens_out != 0
            || cache_read_tokens != 0
            || cache_write_tokens != 0;
        match total_cost_usd {
            Some(cost) if self.cost_known => self.total_cost_usd += cost,
            Some(_) => {}
            None if has_usage => self.cost_known = false,
            None => {}
        }
    }

    fn into_series_point(self, day: String) -> SeriesPointDto {
        SeriesPointDto {
            day,
            session_count: self.session_count,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            total_cost_usd: self.cost_known.then_some(self.total_cost_usd),
        }
    }

    fn into_breakdown_point(self, group_key: String, day: String) -> BreakdownPointDto {
        BreakdownPointDto {
            group_key,
            day,
            session_count: self.session_count,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            total_cost_usd: self.cost_known.then_some(self.total_cost_usd),
        }
    }
}

fn empty_accumulator() -> UsageAccumulator {
    UsageAccumulator {
        cost_known: true,
        ..UsageAccumulator::default()
    }
}

fn week_start(date: time::Date) -> Result<time::Date, (StatusCode, String)> {
    let offset = match date.weekday() {
        time::Weekday::Monday => 0,
        time::Weekday::Tuesday => 1,
        time::Weekday::Wednesday => 2,
        time::Weekday::Thursday => 3,
        time::Weekday::Friday => 4,
        time::Weekday::Saturday => 5,
        time::Weekday::Sunday => 6,
    };
    time::Date::from_julian_day(date.to_julian_day() - offset).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "invalid date range: week bucket is out of range".to_string(),
        )
    })
}

fn period_start(day: &str, granularity: Granularity) -> Result<String, (StatusCode, String)> {
    let date = parse_iso_date_prefix("day", day)?;
    let start = match granularity {
        Granularity::Day => date,
        Granularity::Week => week_start(date)?,
        Granularity::Month => time::Date::from_calendar_date(date.year(), date.month(), 1)
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "invalid date range: month bucket is out of range".to_string(),
                )
            })?,
    };
    Ok(format_date(start))
}

fn rollup_series(
    rows: Vec<DailySeriesRow>,
    granularity: Granularity,
) -> Result<Vec<SeriesPointDto>, (StatusCode, String)> {
    if granularity == Granularity::Day {
        return Ok(rows.into_iter().map(Into::into).collect());
    }

    let mut by_period: BTreeMap<String, UsageAccumulator> = BTreeMap::new();
    for row in rows {
        let period = period_start(&row.day, granularity)?;
        by_period
            .entry(period)
            .or_insert_with(empty_accumulator)
            .add_values(
                row.session_count,
                row.tokens_in,
                row.tokens_out,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.total_cost_usd,
            );
    }

    Ok(by_period
        .into_iter()
        .map(|(day, acc)| acc.into_series_point(day))
        .collect())
}

fn rollup_breakdown(
    rows: Vec<BreakdownRow>,
    granularity: Granularity,
) -> Result<Vec<BreakdownPointDto>, (StatusCode, String)> {
    if granularity == Granularity::Day {
        let mut rows: Vec<_> = rows.into_iter().map(Into::into).collect();
        rows.sort_by(|a: &BreakdownPointDto, b| {
            a.group_key
                .cmp(&b.group_key)
                .then_with(|| a.day.cmp(&b.day))
        });
        return Ok(rows);
    }

    let mut by_group_period: BTreeMap<(String, String), UsageAccumulator> = BTreeMap::new();
    for row in rows {
        let period = period_start(&row.day, granularity)?;
        by_group_period
            .entry((row.group_key, period))
            .or_insert_with(empty_accumulator)
            .add_values(
                row.session_count,
                row.tokens_in,
                row.tokens_out,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.total_cost_usd,
            );
    }

    Ok(by_group_period
        .into_iter()
        .map(|((group_key, day), acc)| acc.into_breakdown_point(group_key, day))
        .collect())
}

/// Per-model effectiveness row.  Computed over worker sessions only.
///
/// Completed-task attribution uses shared-credit: every model that ran at
/// least one worker session on a completed task receives credit for that task.
/// The `completed_task_count` field reflects this shared-credit semantics;
/// UI code should label it accordingly (e.g. "Tasks (shared credit)").
#[derive(Serialize)]
struct ModelEffectivenessDto {
    model_id: String,
    sessions: i64,
    /// Aggregate spend in USD. NULL when all worker sessions for this model
    /// used unpriced models.
    spend_usd: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
    /// Shared-credit completed-task count.
    completed_task_count: i64,
    success_rate: Option<f64>,
    avg_reopens: Option<f64>,
    verification_pass_rate: Option<f64>,
    /// Cost per completed task. NULL when no completed tasks or unpriced.
    cost_per_completed_task: Option<f64>,
    /// Average total tokens per completed task.
    tokens_per_task: Option<f64>,
}

impl From<ModelEffectivenessRow> for ModelEffectivenessDto {
    fn from(r: ModelEffectivenessRow) -> Self {
        Self {
            model_id: r.model_id,
            sessions: r.sessions,
            spend_usd: r.spend_usd,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            completed_task_count: r.shared_credit_completed_task_count,
            success_rate: r.success_rate,
            avg_reopens: r.avg_reopens,
            verification_pass_rate: r.verification_pass_rate,
            cost_per_completed_task: r.cost_per_completed_task,
            tokens_per_task: r.tokens_per_task,
        }
    }
}

/// Project × model matrix entry for frontend consumption.
#[derive(Serialize)]
struct ProjectModelMatrixDto {
    project_id: String,
    model_id: String,
    sessions: i64,
    spend_usd: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
}

impl From<ProjectModelMatrixRow> for ProjectModelMatrixDto {
    fn from(r: ProjectModelMatrixRow) -> Self {
        Self {
            project_id: r.project_id,
            model_id: r.model_id,
            sessions: r.sessions,
            spend_usd: r.spend_usd,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
        }
    }
}

#[derive(Serialize)]
struct UsageResponse {
    /// Echoes the requested granularity so the UI can label axes.
    granularity: Granularity,
    /// Overall totals across the queried window.
    totals: TotalsDto,
    /// Totals for the preceding window of equal length.
    previous_totals: TotalsDto,
    /// Time series for the window at the requested period granularity.
    series: Vec<SeriesPointDto>,
    /// Breakdown rows grouped by the requested dimension and period.
    breakdown: Vec<BreakdownPointDto>,
    /// Per-model effectiveness metrics (worker-scoped, shared-credit attribution).
    model_effectiveness: Vec<ModelEffectivenessDto>,
    /// Project × model spend/token matrix.
    project_model_matrix: Vec<ProjectModelMatrixDto>,
}

impl UsageResponse {
    fn from_results(
        result: UsageAnalyticsResult,
        previous_totals: UsageTotals,
        granularity: Granularity,
        effectiveness_rows: Vec<ModelEffectivenessRow>,
        matrix_rows: Vec<ProjectModelMatrixRow>,
    ) -> Result<Self, (StatusCode, String)> {
        let UsageAnalyticsResult {
            totals,
            series,
            breakdown,
        } = result;
        Ok(Self {
            granularity,
            totals: totals.into(),
            previous_totals: previous_totals.into(),
            series: rollup_series(series, granularity)?,
            breakdown: rollup_breakdown(breakdown, granularity)?,
            model_effectiveness: effectiveness_rows.into_iter().map(Into::into).collect(),
            project_model_matrix: matrix_rows.into_iter().map(Into::into).collect(),
        })
    }
}

// ── Handler ──────────────────────────────────────────────────────────────────

/// `GET /api/admin/usage` — admin-only aggregate usage analytics.
///
/// Gated with [`require_admin`] inside the handler (not a UI route guard) so
/// non-admin API calls are rejected independently of client-side routing.
async fn usage_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UsageQuery>,
) -> Result<Json<UsageResponse>, (StatusCode, String)> {
    require_admin(&state, &headers).await?;
    let (query, granularity) = q.into_typed()?;

    let previous_query = previous_window_query(&query)?;
    let repo = UsageAnalyticsRepository::new(state.db().clone());
    let result = repo
        .query(&query)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let previous = repo
        .query(&previous_query)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Model effectiveness (worker-scoped) and project × model matrix.
    let (effectiveness_rows, matrix_rows) = repo
        .query_effectiveness(&query)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(UsageResponse::from_results(
        result,
        previous.totals,
        granularity,
        effectiveness_rows,
        matrix_rows,
    )?))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn granularity_parses_known_values() {
        assert_eq!(Granularity::parse(None).unwrap(), Granularity::Day);
        assert_eq!(Granularity::parse(Some("day")).unwrap(), Granularity::Day);
        assert_eq!(Granularity::parse(Some("DAY")).unwrap(), Granularity::Day);
        assert_eq!(Granularity::parse(Some("week")).unwrap(), Granularity::Week);
        assert_eq!(
            Granularity::parse(Some("month")).unwrap(),
            Granularity::Month
        );
    }

    #[test]
    fn granularity_rejects_unknown() {
        let err = Granularity::parse(Some("hour")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("hour"));
    }

    #[test]
    fn group_by_parses_all_dimensions() {
        assert_eq!(parse_group_by(None).unwrap(), GroupDimension::Model);
        assert_eq!(
            parse_group_by(Some("model")).unwrap(),
            GroupDimension::Model
        );
        assert_eq!(
            parse_group_by(Some("project")).unwrap(),
            GroupDimension::Project
        );
        assert_eq!(parse_group_by(Some("user")).unwrap(), GroupDimension::User);
        assert_eq!(
            parse_group_by(Some("proposal")).unwrap(),
            GroupDimension::Proposal
        );
        assert_eq!(parse_group_by(Some("task")).unwrap(), GroupDimension::Task);
        assert_eq!(
            parse_group_by(Some("agent")).unwrap(),
            GroupDimension::Agent
        );
    }

    #[test]
    fn group_by_rejects_unknown() {
        let err = parse_group_by(Some("foo")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("foo"));
    }

    #[test]
    fn into_typed_applies_defaults_and_filters_blanks() {
        let q = UsageQuery {
            from: Some("2025-01-01".into()),
            to: Some("2025-02-01".into()),
            granularity: Some("day".into()),
            group_by: Some("model".into()),
            project_id: Some("proj-1".into()),
            model_id: Some("".into()),
            agent_type: None,
        };
        let (typed, gran) = q.into_typed().unwrap();
        assert_eq!(typed.from, "2025-01-01");
        assert_eq!(typed.to, "2025-02-01");
        assert_eq!(typed.group_by, GroupDimension::Model);
        assert_eq!(typed.project_id.as_deref(), Some("proj-1"));
        assert!(typed.model_id.is_none(), "blank strings must be dropped");
        assert!(typed.agent_type.is_none());
        assert_eq!(gran, Granularity::Day);
    }

    #[test]
    fn into_typed_supplies_default_window_when_omitted() {
        let q = UsageQuery {
            from: None,
            to: None,
            granularity: None,
            group_by: None,
            project_id: None,
            model_id: None,
            agent_type: None,
        };
        let (typed, gran) = q.into_typed().unwrap();
        assert!(!typed.from.is_empty());
        assert!(!typed.to.is_empty());
        assert_eq!(gran, Granularity::Day);
        assert_eq!(typed.group_by, GroupDimension::Model);
    }

    #[test]
    fn into_typed_rejects_invalid_date_bounds() {
        let q = UsageQuery {
            from: Some("2025-02-30".into()),
            to: Some("2025-03-01".into()),
            granularity: None,
            group_by: None,
            project_id: None,
            model_id: None,
            agent_type: None,
        };

        let err = q.into_typed().unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("from"));
    }

    #[test]
    fn into_typed_rejects_reversed_date_range() {
        let q = UsageQuery {
            from: Some("2025-03-01".into()),
            to: Some("2025-03-01".into()),
            granularity: None,
            group_by: None,
            project_id: None,
            model_id: None,
            agent_type: None,
        };

        let err = q.into_typed().unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("from must be before to"));
    }

    #[test]
    fn totals_dto_preserves_null_cost() {
        let dto: TotalsDto = UsageTotals {
            session_count: 3,
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            total_cost_usd: None,
        }
        .into();
        assert_eq!(dto.session_count, 3);
        assert!(dto.total_cost_usd.is_none());

        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("total_cost_usd").unwrap().is_null());
    }

    #[test]
    fn series_dto_preserves_cost() {
        let dto: SeriesPointDto = DailySeriesRow {
            day: "2025-03-14".into(),
            session_count: 1,
            tokens_in: 2,
            tokens_out: 3,
            cache_read_tokens: 4,
            cache_write_tokens: 5,
            total_cost_usd: Some(1.23),
        }
        .into();
        assert_eq!(dto.day, "2025-03-14");
        assert_eq!(dto.total_cost_usd, Some(1.23));
    }

    #[test]
    fn response_shape_has_all_fields() {
        let result = UsageAnalyticsResult::default();
        let resp = UsageResponse::from_results(
            result,
            UsageTotals::default(),
            Granularity::Day,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let json = serde_json::to_value(&resp).unwrap();
        for field in [
            "totals",
            "previous_totals",
            "series",
            "breakdown",
            "model_effectiveness",
            "project_model_matrix",
            "granularity",
        ] {
            assert!(json.get(field).is_some(), "missing response field: {field}");
        }
        assert!(json.get("series").unwrap().is_array());
        assert!(json.get("model_effectiveness").unwrap().is_array());
        assert!(json.get("project_model_matrix").unwrap().is_array());
    }

    #[test]
    fn model_effectiveness_dto_has_all_metric_fields() {
        // Verify that a ModelEffectivenessDto serialises with all required
        // metric fields, including shared-credit completed_task_count,
        // avg_reopens, and tokens_per_task.
        let row = ModelEffectivenessRow {
            model_id: "test-model".into(),
            sessions: 10,
            spend_usd: Some(2.50),
            tokens_in: 1000,
            tokens_out: 500,
            shared_credit_completed_task_count: 3,
            success_rate: Some(0.67),
            avg_reopens: Some(0.5),
            verification_pass_rate: Some(1.0),
            cost_per_completed_task: Some(0.83),
            tokens_per_task: Some(500.0),
        };
        let dto: ModelEffectivenessDto = row.into();
        let json = serde_json::to_value(&dto).unwrap();

        assert_eq!(
            json.get("model_id").unwrap().as_str().unwrap(),
            "test-model"
        );
        assert_eq!(json.get("sessions").unwrap().as_i64().unwrap(), 10);
        assert!((json.get("spend_usd").unwrap().as_f64().unwrap() - 2.50).abs() < 0.01);
        assert_eq!(json.get("tokens_in").unwrap().as_i64().unwrap(), 1000);
        assert_eq!(json.get("tokens_out").unwrap().as_i64().unwrap(), 500);
        // Shared-credit completed task count — field name documents the
        // attribution semantics so the UI can label it accurately.
        assert_eq!(
            json.get("completed_task_count").unwrap().as_i64().unwrap(),
            3
        );
        assert!((json.get("success_rate").unwrap().as_f64().unwrap() - 0.67).abs() < 0.01);
        assert!((json.get("avg_reopens").unwrap().as_f64().unwrap() - 0.5).abs() < 0.01);
        assert!(
            (json
                .get("verification_pass_rate")
                .unwrap()
                .as_f64()
                .unwrap()
                - 1.0)
                .abs()
                < 0.01
        );
        assert!(
            (json
                .get("cost_per_completed_task")
                .unwrap()
                .as_f64()
                .unwrap()
                - 0.83)
                .abs()
                < 0.01
        );
        assert!((json.get("tokens_per_task").unwrap().as_f64().unwrap() - 500.0).abs() < 0.01);
    }

    #[test]
    fn model_effectiveness_dto_preserves_null_spend() {
        // When all worker sessions use unpriced models, spend and
        // cost_per_completed_task must be NULL (not zero).
        let row = ModelEffectivenessRow {
            model_id: "unpriced-model".into(),
            sessions: 5,
            spend_usd: None,
            tokens_in: 200,
            tokens_out: 100,
            shared_credit_completed_task_count: 2,
            success_rate: Some(1.0),
            avg_reopens: Some(0.0),
            verification_pass_rate: Some(1.0),
            cost_per_completed_task: None,
            tokens_per_task: Some(150.0),
        };
        let dto: ModelEffectivenessDto = row.into();
        let json = serde_json::to_value(&dto).unwrap();

        assert!(json.get("spend_usd").unwrap().is_null());
        assert!(json.get("cost_per_completed_task").unwrap().is_null());
        assert!(!json.get("tokens_per_task").unwrap().is_null());
    }

    #[test]
    fn project_model_matrix_dto_serialises_correctly() {
        let row = ProjectModelMatrixRow {
            project_id: "proj-1".into(),
            model_id: "model-a".into(),
            sessions: 7,
            spend_usd: Some(1.23),
            tokens_in: 300,
            tokens_out: 150,
        };
        let dto: ProjectModelMatrixDto = row.into();
        let json = serde_json::to_value(&dto).unwrap();

        assert_eq!(json.get("project_id").unwrap().as_str().unwrap(), "proj-1");
        assert_eq!(json.get("model_id").unwrap().as_str().unwrap(), "model-a");
        assert_eq!(json.get("sessions").unwrap().as_i64().unwrap(), 7);
        assert!((json.get("spend_usd").unwrap().as_f64().unwrap() - 1.23).abs() < 0.01);
        assert_eq!(json.get("tokens_in").unwrap().as_i64().unwrap(), 300);
        assert_eq!(json.get("tokens_out").unwrap().as_i64().unwrap(), 150);
    }

    #[test]
    fn previous_window_matches_requested_day_span() {
        let query = UsageAnalyticsQuery {
            from: "2025-03-10".into(),
            to: "2025-03-17".into(),
            group_by: GroupDimension::Project,
            project_id: Some("proj-1".into()),
            model_id: Some("model-1".into()),
            agent_type: Some("worker".into()),
        };

        let previous = previous_window_query(&query).unwrap();
        assert_eq!(previous.from, "2025-03-03");
        assert_eq!(previous.to, "2025-03-10");
        assert_eq!(previous.group_by, GroupDimension::Project);
        assert_eq!(previous.project_id.as_deref(), Some("proj-1"));
        assert_eq!(previous.model_id.as_deref(), Some("model-1"));
        assert_eq!(previous.agent_type.as_deref(), Some("worker"));
    }

    #[test]
    fn weekly_rollup_sums_daily_rows_and_preserves_unknown_cost() {
        let rows = vec![
            DailySeriesRow {
                day: "2025-03-03".into(),
                session_count: 1,
                tokens_in: 10,
                tokens_out: 20,
                cache_read_tokens: 3,
                cache_write_tokens: 4,
                total_cost_usd: Some(0.5),
            },
            DailySeriesRow {
                day: "2025-03-05".into(),
                session_count: 2,
                tokens_in: 30,
                tokens_out: 40,
                cache_read_tokens: 5,
                cache_write_tokens: 6,
                total_cost_usd: None,
            },
            DailySeriesRow {
                day: "2025-03-10".into(),
                session_count: 3,
                tokens_in: 50,
                tokens_out: 60,
                cache_read_tokens: 7,
                cache_write_tokens: 8,
                total_cost_usd: Some(1.5),
            },
        ];

        let rolled = rollup_series(rows, Granularity::Week).unwrap();
        assert_eq!(rolled.len(), 2);
        assert_eq!(rolled[0].day, "2025-03-03");
        assert_eq!(rolled[0].session_count, 3);
        assert_eq!(rolled[0].tokens_in, 40);
        assert!(rolled[0].total_cost_usd.is_none());
        assert_eq!(rolled[1].day, "2025-03-10");
        assert_eq!(rolled[1].total_cost_usd, Some(1.5));
    }

    #[test]
    fn monthly_breakdown_rollup_keeps_groups_separate_and_sorted() {
        let rows = vec![
            BreakdownRow {
                group_key: "b".into(),
                day: "2025-02-01".into(),
                session_count: 1,
                tokens_in: 2,
                tokens_out: 3,
                cache_read_tokens: 4,
                cache_write_tokens: 5,
                total_cost_usd: Some(0.1),
            },
            BreakdownRow {
                group_key: "a".into(),
                day: "2025-01-31".into(),
                session_count: 2,
                tokens_in: 3,
                tokens_out: 4,
                cache_read_tokens: 5,
                cache_write_tokens: 6,
                total_cost_usd: Some(0.2),
            },
            BreakdownRow {
                group_key: "a".into(),
                day: "2025-01-15".into(),
                session_count: 3,
                tokens_in: 4,
                tokens_out: 5,
                cache_read_tokens: 6,
                cache_write_tokens: 7,
                total_cost_usd: Some(0.3),
            },
        ];

        let rolled = rollup_breakdown(rows, Granularity::Month).unwrap();
        assert_eq!(rolled.len(), 2);
        assert_eq!(rolled[0].group_key, "a");
        assert_eq!(rolled[0].day, "2025-01-01");
        assert_eq!(rolled[0].session_count, 5);
        assert_eq!(rolled[0].tokens_in, 7);
        assert_eq!(rolled[0].total_cost_usd, Some(0.5));
        assert_eq!(rolled[1].group_key, "b");
        assert_eq!(rolled[1].day, "2025-02-01");
    }

    #[test]
    fn previous_totals_uses_supplied_repository_totals() {
        let previous_totals = UsageTotals {
            session_count: 2,
            tokens_in: 11,
            tokens_out: 12,
            cache_read_tokens: 13,
            cache_write_tokens: 14,
            total_cost_usd: Some(0.42),
        };
        let resp = UsageResponse::from_results(
            UsageAnalyticsResult::default(),
            previous_totals,
            Granularity::Day,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(resp.previous_totals.session_count, 2);
        assert_eq!(resp.previous_totals.tokens_in, 11);
        assert_eq!(resp.previous_totals.total_cost_usd, Some(0.42));
    }
}
