// HTTP handler for the `/api/admin/usage` REST endpoint consumed by the
// admin analytics UI.
//
// Emits exactly the shape the dashboard consumes:
//   `kpis`, `time_series`, `breakdowns` (by_user/by_project/by_proposal/by_task),
//   `model_effectiveness`, `project_model_matrix`, and `generated_at`.
//
// The repository returns daily multi-dimensional series rows; this handler
// maps those ISO day buckets into requested day/week/month periods using
// deterministic Rust date arithmetic, derives the KPI cards from the current
// and previous window totals, and renames repository fields to the frontend
// contract.

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
    EntityBreakdownRow, GroupDimension, ModelEffectivenessRow, ProjectModelMatrixRow,
    SeriesDetailRow, UsageAnalyticsQuery, UsageAnalyticsRepository, UsageTotals,
};

pub(super) fn router() -> Router<AppState> {
    // Namespaced under `/api/admin` so it cannot shadow the SPA client-side
    // route of the same name: a hard refresh on `/admin/usage` must fall
    // through to the static `index.html` fallback, not hit this JSON handler.
    Router::new().route("/api/admin/usage", get(usage_handler))
}

// ── Query parsing ────────────────────────────────────────────────────────────

/// Raw query params deserialised from the request URL.  Mirrors the frontend
/// `UsageAnalyticsFilters` exactly.  All are optional; defaults and validation
/// are applied in [`UsageQuery::into_typed`].
#[derive(Debug, Deserialize)]
struct UsageQuery {
    /// Shorthand date range; mutually exclusive with start/end.
    preset: Option<String>,
    /// ISO start date (inclusive); used only when `preset` is absent.
    start: Option<String>,
    /// ISO end date (exclusive); used only when `preset` is absent.
    end: Option<String>,
    granularity: Option<String>,
    project_id: Option<String>,
    /// Model identifier filter.
    model: Option<String>,
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

/// A `from`/`to` window ending now and spanning `days` days, in RFC3339.
fn window_days(days: i64) -> (String, String) {
    use time::format_description::well_known::Rfc3339;
    let now = time::OffsetDateTime::now_utc();
    let start = now - time::Duration::days(days);
    let to = now.format(&Rfc3339).unwrap_or_default();
    let from = start.format(&Rfc3339).unwrap_or_default();
    (from, to)
}

/// Validate an HTTP date bound while preserving the original string passed to
/// the repository. The DB query compares ISO-8601 strings, so callers may pass
/// either a date (`YYYY-MM-DD`) or a timestamp whose first 10 characters are an
/// ISO date prefix.
fn parse_iso_date_prefix(name: &str, value: &str) -> Result<time::Date, (StatusCode, String)> {
    let invalid = || {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid {name}: expected YYYY-MM-DD or ISO-8601 timestamp"),
        )
    };

    let Some(date) = value.get(..10) else {
        return Err(invalid());
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
        return Err(invalid());
    }

    let year = date[0..4].parse::<i32>().map_err(|_| invalid())?;
    let month = date[5..7].parse::<u8>().map_err(|_| invalid())?;
    let day = date[8..10].parse::<u8>().map_err(|_| invalid())?;
    let month = time::Month::try_from(month).map_err(|_| invalid())?;
    time::Date::from_calendar_date(year, month, day).map_err(|_| invalid())
}

fn validate_date_range(from: String, to: String) -> Result<(String, String), (StatusCode, String)> {
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
    /// Resolve the effective `from`/`to` window from `preset` or `start`/`end`,
    /// applying a default 30-day window when nothing is supplied.
    fn resolve_window(&self) -> Result<(String, String), (StatusCode, String)> {
        if let Some(preset) = self.preset.as_deref().filter(|s| !s.is_empty()) {
            let days = match preset.to_ascii_lowercase().as_str() {
                "7d" => 7,
                "30d" => 30,
                other => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("invalid preset '{other}' (expected 7d or 30d)"),
                    ));
                }
            };
            return Ok(window_days(days));
        }

        let (default_from, default_to) = window_days(30);
        let from = self
            .start
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or(default_from);
        let to = self
            .end
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or(default_to);
        validate_date_range(from, to)
    }

    /// Validate the raw params and produce the typed repository query plus the
    /// parsed granularity.  Returns an HTTP-style error tuple on bad input.
    fn into_typed(self) -> Result<(UsageAnalyticsQuery, Granularity), (StatusCode, String)> {
        let granularity = Granularity::parse(self.granularity.as_deref())?;
        let (from, to) = self.resolve_window()?;

        Ok((
            UsageAnalyticsQuery {
                from,
                to,
                // The dashboard computes all four entity breakdowns; group_by is
                // retained only for repository API compatibility.
                group_by: GroupDimension::Model,
                project_id: self.project_id.filter(|s| !s.is_empty()),
                model_id: self.model.filter(|s| !s.is_empty()),
                agent_type: self.agent_type.filter(|s| !s.is_empty()),
            },
            granularity,
        ))
    }
}

// ── Response DTOs ────────────────────────────────────────────────────────────

/// A single KPI card derived from the current vs previous window totals.
#[derive(Serialize)]
struct UsageKpiDto {
    label: String,
    /// Numeric value; `null` when unavailable (e.g. unpriced spend).
    value: Option<f64>,
    /// Period-over-period change as a fraction (0.12 == +12%); `null` when the
    /// previous window was empty.
    delta_pct: Option<f64>,
    /// Pre-formatted display value (currency); empty string lets the UI fall
    /// back to compact-number formatting of `value`.
    formatted: String,
}

/// A point in the multi-dimensional time series.  Carries the model / project /
/// agent dimensions so the Overview tab can group spend client-side.
#[derive(Serialize)]
struct SeriesPointDto {
    date: String,
    /// Spend in USD; `null` when any session in the bucket was unpriced.
    cost: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
    task_count: i64,
    model: String,
    project_id: String,
    project_name: String,
    agent_type: String,
}

/// A breakdown row for one entity (user / project / proposal / task).
#[derive(Serialize)]
struct BreakdownRowDto {
    id: String,
    name: String,
    cost: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
    task_count: i64,
    success_rate: Option<f64>,
    avg_reopens: Option<f64>,
    cost_per_task: Option<f64>,
    /// Present only for the by_task breakdown; lets the UI build a task link.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// Present only for the by_proposal breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    proposal_id: Option<String>,
}

#[derive(Serialize)]
struct BreakdownsDto {
    by_user: Vec<BreakdownRowDto>,
    by_project: Vec<BreakdownRowDto>,
    by_proposal: Vec<BreakdownRowDto>,
    by_task: Vec<BreakdownRowDto>,
}

/// Per-model effectiveness, renamed to the frontend contract.
#[derive(Serialize)]
struct ModelEffectivenessDto {
    model: String,
    task_count: i64,
    success_rate: Option<f64>,
    avg_reopens: Option<f64>,
    cost_per_task: Option<f64>,
    total_cost: Option<f64>,
    total_tokens: i64,
    tokens_in: i64,
    tokens_out: i64,
    session_count: i64,
    /// Shared-credit completed-task count (== task_count); surfaced separately
    /// so the UI can label the attribution semantics.
    completed_task_count: i64,
}

impl From<ModelEffectivenessRow> for ModelEffectivenessDto {
    fn from(r: ModelEffectivenessRow) -> Self {
        Self {
            model: r.model_id,
            task_count: r.shared_credit_completed_task_count,
            success_rate: r.success_rate,
            avg_reopens: r.avg_reopens,
            cost_per_task: r.cost_per_completed_task,
            total_cost: r.spend_usd,
            total_tokens: r.tokens_in + r.tokens_out,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            session_count: r.sessions,
            completed_task_count: r.shared_credit_completed_task_count,
        }
    }
}

/// Project × model matrix cell, renamed to the frontend contract.
#[derive(Serialize)]
struct ProjectModelCellDto {
    project_id: String,
    project_name: String,
    model: String,
    cost_per_task: Option<f64>,
    success_rate: Option<f64>,
    avg_reopens: Option<f64>,
    total_cost: Option<f64>,
    total_tokens: i64,
}

impl From<ProjectModelMatrixRow> for ProjectModelCellDto {
    fn from(r: ProjectModelMatrixRow) -> Self {
        let cost_per_task = match (r.spend_usd, r.task_count) {
            (Some(cost), n) if n > 0 => Some(cost / n as f64),
            _ => None,
        };
        Self {
            project_id: r.project_id,
            project_name: r.project_name,
            model: r.model_id,
            cost_per_task,
            success_rate: r.success_rate,
            avg_reopens: r.avg_reopens,
            total_cost: r.spend_usd,
            total_tokens: r.tokens_in + r.tokens_out,
        }
    }
}

#[derive(Serialize)]
struct UsageResponse {
    kpis: Vec<UsageKpiDto>,
    time_series: Vec<SeriesPointDto>,
    breakdowns: BreakdownsDto,
    model_effectiveness: Vec<ModelEffectivenessDto>,
    project_model_matrix: Vec<ProjectModelCellDto>,
    /// RFC3339 timestamp when the response was generated.
    generated_at: String,
}

// ── Derivations ──────────────────────────────────────────────────────────────

/// Currency formatting that mirrors the frontend `formatCurrency` precision.
fn format_currency(value: f64) -> String {
    if value >= 100.0 {
        format!("${value:.0}")
    } else {
        format!("${value:.2}")
    }
}

/// Period-over-period change as a fraction; `None` when the prior value is ~0.
fn pct_delta(current: f64, previous: f64) -> Option<f64> {
    if previous.abs() < f64::EPSILON {
        None
    } else {
        Some((current - previous) / previous)
    }
}

/// Build the KPI cards from current and previous window totals.
fn build_kpis(totals: &UsageTotals, previous: &UsageTotals) -> Vec<UsageKpiDto> {
    let spend_delta = match (totals.total_cost_usd, previous.total_cost_usd) {
        (Some(cur), Some(prev)) => pct_delta(cur, prev),
        _ => None,
    };

    let tokens = (totals.tokens_in + totals.tokens_out) as f64;
    let prev_tokens = (previous.tokens_in + previous.tokens_out) as f64;

    vec![
        UsageKpiDto {
            label: "Spend".to_string(),
            value: totals.total_cost_usd,
            delta_pct: spend_delta,
            formatted: totals.total_cost_usd.map(format_currency).unwrap_or_default(),
        },
        UsageKpiDto {
            label: "Tokens".to_string(),
            value: Some(tokens),
            delta_pct: pct_delta(tokens, prev_tokens),
            formatted: String::new(),
        },
        UsageKpiDto {
            label: "Sessions".to_string(),
            value: Some(totals.session_count as f64),
            delta_pct: pct_delta(totals.session_count as f64, previous.session_count as f64),
            formatted: String::new(),
        },
        UsageKpiDto {
            label: "Cache reads".to_string(),
            value: Some(totals.cache_read_tokens as f64),
            delta_pct: pct_delta(
                totals.cache_read_tokens as f64,
                previous.cache_read_tokens as f64,
            ),
            formatted: String::new(),
        },
    ]
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

/// Accumulates spend (preserving NULL-unpriced semantics) and token/task
/// counters for a rolled-up series bucket.
struct SeriesAccumulator {
    tokens_in: i64,
    tokens_out: i64,
    task_count: i64,
    cost_sum: f64,
    cost_known: bool,
    any_usage: bool,
}

impl Default for SeriesAccumulator {
    fn default() -> Self {
        // `cost_known` starts true and flips to false the first time an
        // unpriced (NULL cost) row with usage is folded in.
        Self {
            tokens_in: 0,
            tokens_out: 0,
            task_count: 0,
            cost_sum: 0.0,
            cost_known: true,
            any_usage: false,
        }
    }
}

impl SeriesAccumulator {
    fn add(&mut self, row: &SeriesDetailRow) {
        self.tokens_in += row.tokens_in;
        self.tokens_out += row.tokens_out;
        self.task_count += row.task_count;
        self.any_usage = true;
        match row.cost {
            Some(cost) if self.cost_known => self.cost_sum += cost,
            Some(_) => {}
            None => self.cost_known = false,
        }
    }

    fn cost(&self) -> Option<f64> {
        if self.cost_known && self.any_usage {
            Some(self.cost_sum)
        } else {
            None
        }
    }
}

/// Roll up the daily multi-dimensional series into the requested granularity,
/// grouping by (period, model, project, agent).
fn rollup_series(
    rows: Vec<SeriesDetailRow>,
    granularity: Granularity,
) -> Result<Vec<SeriesPointDto>, (StatusCode, String)> {
    // Key: (period, model, project_id, project_name, agent_type). project_name
    // is part of the key purely to carry it through; it is functionally
    // dependent on project_id.
    type Key = (String, String, String, String, String);
    let mut buckets: BTreeMap<Key, SeriesAccumulator> = BTreeMap::new();

    for row in rows {
        let period = period_start(&row.day, granularity)?;
        let key = (
            period,
            row.model.clone(),
            row.project_id.clone(),
            row.project_name.clone(),
            row.agent_type.clone(),
        );
        buckets.entry(key).or_default().add(&row);
    }

    Ok(buckets
        .into_iter()
        .map(|((date, model, project_id, project_name, agent_type), acc)| SeriesPointDto {
            date,
            cost: acc.cost(),
            tokens_in: acc.tokens_in,
            tokens_out: acc.tokens_out,
            task_count: acc.task_count,
            model,
            project_id,
            project_name,
            agent_type,
        })
        .collect())
}

/// Map a repository breakdown row to the response DTO, deriving cost-per-task
/// and attaching the entity-specific link id.
fn breakdown_row(row: EntityBreakdownRow, dimension: GroupDimension) -> BreakdownRowDto {
    let cost_per_task = match (row.cost, row.task_count) {
        (Some(cost), n) if n > 0 => Some(cost / n as f64),
        _ => None,
    };
    let (task_id, proposal_id) = match dimension {
        GroupDimension::Task => (Some(row.id.clone()), None),
        GroupDimension::Proposal => (None, Some(row.id.clone())),
        _ => (None, None),
    };
    BreakdownRowDto {
        id: row.id,
        name: row.name,
        cost: row.cost,
        tokens_in: row.tokens_in,
        tokens_out: row.tokens_out,
        task_count: row.task_count,
        success_rate: row.success_rate,
        avg_reopens: row.avg_reopens,
        cost_per_task,
        task_id,
        proposal_id,
    }
}

fn breakdown_rows(rows: Vec<EntityBreakdownRow>, dimension: GroupDimension) -> Vec<BreakdownRowDto> {
    rows.into_iter()
        .map(|row| breakdown_row(row, dimension))
        .collect()
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
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
    let internal = |e: djinn_db::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());

    let totals = repo.totals(&query).await.map_err(internal)?;
    let previous = repo.totals(&previous_query).await.map_err(internal)?;
    let series = repo.series_detailed(&query).await.map_err(internal)?;

    let by_user = repo
        .entity_breakdown(&query, GroupDimension::User)
        .await
        .map_err(internal)?;
    let by_project = repo
        .entity_breakdown(&query, GroupDimension::Project)
        .await
        .map_err(internal)?;
    let by_proposal = repo
        .entity_breakdown(&query, GroupDimension::Proposal)
        .await
        .map_err(internal)?;
    let by_task = repo
        .entity_breakdown(&query, GroupDimension::Task)
        .await
        .map_err(internal)?;

    let (effectiveness_rows, matrix_rows) =
        repo.query_effectiveness(&query).await.map_err(internal)?;

    let response = UsageResponse {
        kpis: build_kpis(&totals, &previous),
        time_series: rollup_series(series, granularity)?,
        breakdowns: BreakdownsDto {
            by_user: breakdown_rows(by_user, GroupDimension::User),
            by_project: breakdown_rows(by_project, GroupDimension::Project),
            by_proposal: breakdown_rows(by_proposal, GroupDimension::Proposal),
            by_task: breakdown_rows(by_task, GroupDimension::Task),
        },
        model_effectiveness: effectiveness_rows.into_iter().map(Into::into).collect(),
        project_model_matrix: matrix_rows.into_iter().map(Into::into).collect(),
        generated_at: now_rfc3339(),
    };

    Ok(Json(response))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_query() -> UsageQuery {
        UsageQuery {
            preset: None,
            start: None,
            end: None,
            granularity: None,
            project_id: None,
            model: None,
            agent_type: None,
        }
    }

    #[test]
    fn granularity_parses_known_values() {
        assert_eq!(Granularity::parse(None).unwrap(), Granularity::Day);
        assert_eq!(Granularity::parse(Some("DAY")).unwrap(), Granularity::Day);
        assert_eq!(Granularity::parse(Some("week")).unwrap(), Granularity::Week);
        assert_eq!(Granularity::parse(Some("month")).unwrap(), Granularity::Month);
    }

    #[test]
    fn granularity_rejects_unknown() {
        let err = Granularity::parse(Some("hour")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("hour"));
    }

    #[test]
    fn into_typed_maps_model_filter_and_drops_blanks() {
        let q = UsageQuery {
            start: Some("2025-01-01".into()),
            end: Some("2025-02-01".into()),
            granularity: Some("day".into()),
            project_id: Some("proj-1".into()),
            model: Some("".into()),
            ..empty_query()
        };
        let (typed, gran) = q.into_typed().unwrap();
        assert_eq!(typed.from, "2025-01-01");
        assert_eq!(typed.to, "2025-02-01");
        assert_eq!(typed.project_id.as_deref(), Some("proj-1"));
        assert!(typed.model_id.is_none(), "blank model must be dropped");
        assert_eq!(gran, Granularity::Day);
    }

    #[test]
    fn into_typed_supplies_default_window_when_omitted() {
        let (typed, gran) = empty_query().into_typed().unwrap();
        assert!(!typed.from.is_empty());
        assert!(!typed.to.is_empty());
        assert_eq!(gran, Granularity::Day);
    }

    #[test]
    fn preset_30d_window_is_wider_than_7d() {
        let span = |preset: &str| {
            let q = UsageQuery {
                preset: Some(preset.into()),
                ..empty_query()
            };
            let (typed, _) = q.into_typed().unwrap();
            let from = parse_iso_date_prefix("from", &typed.from).unwrap();
            let to = parse_iso_date_prefix("to", &typed.to).unwrap();
            to.to_julian_day() - from.to_julian_day()
        };
        assert!(span("30d") > span("7d"));
    }

    #[test]
    fn invalid_preset_returns_400() {
        let q = UsageQuery {
            preset: Some("90d".into()),
            ..empty_query()
        };
        let err = q.into_typed().unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("90d"));
    }

    #[test]
    fn into_typed_rejects_reversed_custom_range() {
        let q = UsageQuery {
            start: Some("2025-03-01".into()),
            end: Some("2025-03-01".into()),
            ..empty_query()
        };
        let err = q.into_typed().unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("from must be before to"));
    }

    #[test]
    fn into_typed_rejects_invalid_date() {
        let q = UsageQuery {
            start: Some("2025-02-30".into()),
            end: Some("2025-03-01".into()),
            ..empty_query()
        };
        let err = q.into_typed().unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn previous_window_matches_requested_day_span() {
        let query = UsageAnalyticsQuery {
            from: "2025-03-10".into(),
            to: "2025-03-17".into(),
            group_by: GroupDimension::Model,
            project_id: Some("proj-1".into()),
            model_id: Some("model-1".into()),
            agent_type: Some("worker".into()),
        };
        let previous = previous_window_query(&query).unwrap();
        assert_eq!(previous.from, "2025-03-03");
        assert_eq!(previous.to, "2025-03-10");
        assert_eq!(previous.project_id.as_deref(), Some("proj-1"));
    }

    #[test]
    fn build_kpis_emits_four_cards_with_deltas() {
        let totals = UsageTotals {
            session_count: 10,
            tokens_in: 100,
            tokens_out: 100,
            cache_read_tokens: 50,
            cache_write_tokens: 5,
            total_cost_usd: Some(20.0),
        };
        let previous = UsageTotals {
            session_count: 5,
            tokens_in: 50,
            tokens_out: 50,
            cache_read_tokens: 25,
            cache_write_tokens: 0,
            total_cost_usd: Some(10.0),
        };
        let kpis = build_kpis(&totals, &previous);
        assert_eq!(kpis.len(), 4);

        let spend = &kpis[0];
        assert_eq!(spend.label, "Spend");
        assert_eq!(spend.value, Some(20.0));
        assert!((spend.delta_pct.unwrap() - 1.0).abs() < 1e-9, "doubled spend = +100%");
        assert!(spend.formatted.starts_with('$'));

        let tokens = &kpis[1];
        assert_eq!(tokens.value, Some(200.0));
        assert!(tokens.formatted.is_empty());
    }

    #[test]
    fn build_kpis_null_spend_yields_null_value_and_delta() {
        let totals = UsageTotals {
            total_cost_usd: None,
            ..UsageTotals::default()
        };
        let kpis = build_kpis(&totals, &UsageTotals::default());
        assert_eq!(kpis[0].label, "Spend");
        assert!(kpis[0].value.is_none());
        assert!(kpis[0].delta_pct.is_none());
        assert!(kpis[0].formatted.is_empty());
        // Previous window empty → token delta is null, not a divide-by-zero.
        assert!(kpis[1].delta_pct.is_none());
    }

    fn series_row(day: &str, model: &str, cost: Option<f64>) -> SeriesDetailRow {
        SeriesDetailRow {
            day: day.into(),
            model: model.into(),
            project_id: "p1".into(),
            project_name: "Proj One".into(),
            agent_type: "worker".into(),
            session_count: 1,
            tokens_in: 10,
            tokens_out: 5,
            task_count: 1,
            cost,
        }
    }

    #[test]
    fn rollup_series_day_is_identity_per_dimension() {
        let rows = vec![
            series_row("2025-03-10", "a", Some(1.0)),
            series_row("2025-03-10", "b", Some(2.0)),
        ];
        let points = rollup_series(rows, Granularity::Day).unwrap();
        assert_eq!(points.len(), 2);
        assert!(points.iter().all(|p| p.date == "2025-03-10"));
    }

    #[test]
    fn rollup_series_week_groups_same_dimension_and_preserves_null_cost() {
        let rows = vec![
            series_row("2025-03-03", "a", Some(1.0)),
            series_row("2025-03-05", "a", None),
            series_row("2025-03-04", "b", Some(3.0)),
        ];
        let points = rollup_series(rows, Granularity::Week).unwrap();
        // model a (two rows, one unpriced) collapses to one bucket with null
        // cost; model b stays separate.
        let a = points.iter().find(|p| p.model == "a").unwrap();
        assert_eq!(a.date, "2025-03-03");
        assert_eq!(a.tokens_in, 20);
        assert!(a.cost.is_none(), "any unpriced row makes the bucket null");
        let b = points.iter().find(|p| p.model == "b").unwrap();
        assert_eq!(b.cost, Some(3.0));
    }

    #[test]
    fn breakdown_row_sets_links_and_cost_per_task() {
        let row = EntityBreakdownRow {
            id: "task-1".into(),
            name: "A task".into(),
            cost: Some(4.0),
            tokens_in: 10,
            tokens_out: 10,
            task_count: 2,
            success_rate: Some(0.5),
            avg_reopens: Some(1.0),
        };
        let task = breakdown_row(row.clone(), GroupDimension::Task);
        assert_eq!(task.task_id.as_deref(), Some("task-1"));
        assert!(task.proposal_id.is_none());
        assert_eq!(task.cost_per_task, Some(2.0));

        let proposal = breakdown_row(row.clone(), GroupDimension::Proposal);
        assert_eq!(proposal.proposal_id.as_deref(), Some("task-1"));
        assert!(proposal.task_id.is_none());

        let user = breakdown_row(row, GroupDimension::User);
        assert!(user.task_id.is_none() && user.proposal_id.is_none());
    }

    #[test]
    fn breakdown_row_zero_tasks_has_null_cost_per_task() {
        let row = EntityBreakdownRow {
            id: "u1".into(),
            name: "user".into(),
            cost: Some(3.0),
            tokens_in: 1,
            tokens_out: 1,
            task_count: 0,
            success_rate: None,
            avg_reopens: None,
        };
        let dto = breakdown_row(row, GroupDimension::User);
        assert!(dto.cost_per_task.is_none());
    }

    #[test]
    fn model_effectiveness_dto_renames_to_frontend_contract() {
        let row = ModelEffectivenessRow {
            model_id: "gpt".into(),
            sessions: 4,
            spend_usd: Some(2.0),
            tokens_in: 100,
            tokens_out: 50,
            shared_credit_completed_task_count: 3,
            success_rate: Some(0.75),
            avg_reopens: Some(0.2),
            cost_per_completed_task: Some(0.66),
            tokens_per_task: Some(50.0),
        };
        let dto: ModelEffectivenessDto = row.into();
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json.get("model").unwrap().as_str().unwrap(), "gpt");
        assert_eq!(json.get("task_count").unwrap().as_i64().unwrap(), 3);
        assert_eq!(json.get("completed_task_count").unwrap().as_i64().unwrap(), 3);
        assert_eq!(json.get("session_count").unwrap().as_i64().unwrap(), 4);
        assert_eq!(json.get("total_tokens").unwrap().as_i64().unwrap(), 150);
        assert!((json.get("total_cost").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9);
        assert!((json.get("cost_per_task").unwrap().as_f64().unwrap() - 0.66).abs() < 1e-9);
        // Repository-only names must not leak.
        assert!(json.get("model_id").is_none());
        assert!(json.get("spend_usd").is_none());
    }

    #[test]
    fn project_model_cell_dto_derives_cost_per_task_and_renames() {
        let row = ProjectModelMatrixRow {
            project_id: "p1".into(),
            project_name: "Proj".into(),
            model_id: "m1".into(),
            sessions: 2,
            spend_usd: Some(6.0),
            tokens_in: 30,
            tokens_out: 20,
            task_count: 3,
            success_rate: Some(1.0),
            avg_reopens: Some(0.0),
        };
        let dto: ProjectModelCellDto = row.into();
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json.get("model").unwrap().as_str().unwrap(), "m1");
        assert_eq!(json.get("project_name").unwrap().as_str().unwrap(), "Proj");
        assert_eq!(json.get("total_tokens").unwrap().as_i64().unwrap(), 50);
        assert!((json.get("cost_per_task").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9);
        assert!((json.get("total_cost").unwrap().as_f64().unwrap() - 6.0).abs() < 1e-9);
    }

    #[test]
    fn response_serialises_with_frontend_contract_fields() {
        let response = UsageResponse {
            kpis: build_kpis(&UsageTotals::default(), &UsageTotals::default()),
            time_series: Vec::new(),
            breakdowns: BreakdownsDto {
                by_user: Vec::new(),
                by_project: Vec::new(),
                by_proposal: Vec::new(),
                by_task: Vec::new(),
            },
            model_effectiveness: Vec::new(),
            project_model_matrix: Vec::new(),
            generated_at: "2025-06-20T00:00:00Z".into(),
        };
        let json = serde_json::to_value(&response).unwrap();
        for field in [
            "kpis",
            "time_series",
            "breakdowns",
            "model_effectiveness",
            "project_model_matrix",
            "generated_at",
        ] {
            assert!(json.get(field).is_some(), "missing field {field}");
        }
        let breakdowns = json.get("breakdowns").unwrap();
        for field in ["by_user", "by_project", "by_proposal", "by_task"] {
            assert!(breakdowns.get(field).unwrap().is_array(), "missing {field}");
        }
    }
}
