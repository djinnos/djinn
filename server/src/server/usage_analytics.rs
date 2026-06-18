// HTTP handler for the `/api/admin/usage` REST endpoint consumed by the
// admin analytics UI.
//
// Returns the proposal response shape for usage/cost reporting at daily
// granularity: `totals`, `previous_totals`, `series`, `breakdown`,
// `model_effectiveness`, and `project_model_matrix`.  This task wires the
// daily repository rows from task yzh4 into JSON DTOs; week/month rollups,
// previous-totals computation, and model effectiveness are filled by
// follow-up tasks and left as empty/placeholder here.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::auth::require_admin;
use djinn_db::{
    DailySeriesRow, GroupDimension, UsageAnalyticsQuery, UsageAnalyticsRepository,
    UsageAnalyticsResult, UsageTotals,
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

/// Time-series bucket granularity.  Only `Day` is honoured in this task;
/// `Week` and `Month` are accepted and validated here so the follow-up
/// rollup task can act on them without changing the parser.
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
fn validate_date_bound(name: &str, value: String) -> Result<String, (StatusCode, String)> {
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
    })?;

    Ok(value)
}

fn validate_date_range(from: String, to: String) -> Result<(String, String), (StatusCode, String)> {
    let from = validate_date_bound("from", from)?;
    let to = validate_date_bound("to", to)?;
    if from >= to {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid date range: from must be before to".to_string(),
        ));
    }
    Ok((from, to))
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

impl From<djinn_db::BreakdownRow> for BreakdownPointDto {
    fn from(r: djinn_db::BreakdownRow) -> Self {
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

/// Per-model effectiveness row.  Computed over worker sessions only in the
/// follow-up effectiveness task (0d3p); left as an empty placeholder here so
/// the response shape is stable for the frontend.
#[derive(Serialize)]
struct ModelEffectivenessDto {
    model_id: String,
    sessions: i64,
    spend_usd: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
    completed_task_count: i64,
    success_rate: Option<f64>,
    verification_pass_rate: Option<f64>,
    cost_per_completed_task: Option<f64>,
}

/// Project × model matrix entry.  Populated by the follow-up effectiveness
/// task (0d3p); empty placeholder here.
#[derive(Serialize)]
struct ProjectModelMatrixDto {
    project_id: String,
    model_id: String,
    sessions: i64,
    spend_usd: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
}

#[derive(Serialize)]
struct UsageResponse {
    /// Echoes the requested granularity so the UI can label axes.
    granularity: Granularity,
    /// Overall totals across the queried window.
    totals: TotalsDto,
    /// Totals for the preceding window of equal length.  Populated by the
    /// follow-up rollup task (g86s); zeroed here.
    previous_totals: TotalsDto,
    /// Daily time series for the window.
    series: Vec<SeriesPointDto>,
    /// Breakdown rows grouped by the requested dimension, per day.
    breakdown: Vec<BreakdownPointDto>,
    /// Per-model effectiveness metrics (worker-scoped).  Placeholder until 0d3p.
    model_effectiveness: Vec<ModelEffectivenessDto>,
    /// Project × model spend/token matrix.  Placeholder until 0d3p.
    project_model_matrix: Vec<ProjectModelMatrixDto>,
}

impl UsageResponse {
    fn from_result(result: UsageAnalyticsResult, granularity: Granularity) -> Self {
        let UsageAnalyticsResult {
            totals,
            series,
            breakdown,
        } = result;
        Self {
            granularity,
            totals: totals.into(),
            // previous_totals: follow-up task g86s fills this.
            previous_totals: TotalsDto {
                session_count: 0,
                tokens_in: 0,
                tokens_out: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                total_cost_usd: None,
            },
            series: series.into_iter().map(Into::into).collect(),
            breakdown: breakdown.into_iter().map(Into::into).collect(),
            // model_effectiveness / project_model_matrix: follow-up task 0d3p.
            model_effectiveness: Vec::new(),
            project_model_matrix: Vec::new(),
        }
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

    let repo = UsageAnalyticsRepository::new(state.db().clone());
    let result = repo
        .query(&query)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(UsageResponse::from_result(result, granularity)))
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
        let resp = UsageResponse::from_result(result, Granularity::Day);
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
    fn previous_totals_defaults_to_zero_unpriced() {
        let resp = UsageResponse::from_result(UsageAnalyticsResult::default(), Granularity::Day);
        assert_eq!(resp.previous_totals.session_count, 0);
        assert!(resp.previous_totals.total_cost_usd.is_none());
    }
}
