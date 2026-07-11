// Usage analytics handler for /api/admin/usage — returns aggregate KPIs,
// time-series, and multi-dimensional breakdowns for the admin analytics UI.
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

use schemars::JsonSchema;
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
#[derive(Debug, Deserialize, JsonSchema)]
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
    /// User identifier filter (session creator, falling back to task creator).
    user_id: Option<String>,
}

/// Time-series bucket granularity. Repository rows are daily; week/month
/// variants are rolled up in this handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
        user_id: query.user_id.clone(),
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
                user_id: self.user_id.filter(|s| !s.is_empty()),
            },
            granularity,
        ))
    }
}

// ── Response DTOs ────────────────────────────────────────────────────────────

/// A single KPI card derived from the current vs previous window totals.
#[derive(Serialize, JsonSchema)]
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
    /// Optional qualifier shown under the value (e.g. that spend is an
    /// estimate at API rates, and how many sessions were unpriced). `None`
    /// when the card needs no caption.
    #[serde(skip_serializing_if = "Option::is_none")]
    caption: Option<String>,
    /// Optional composition of the headline value (e.g. the Tokens card split
    /// into input / cached / output). The UI renders these as a small inline
    /// row beneath the value. `None` when the card has no breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    breakdown: Option<Vec<UsageKpiPartDto>>,
    /// Actual API spend contributed by this KPI card's aggregate.
    /// Present only on the "Actual API Spend" card.
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost contributed by this KPI card's
    /// aggregate. Present only on the "Projected Cost" card.
    #[serde(skip_serializing_if = "Option::is_none")]
    projected_usd: Option<f64>,
    /// Combined list-price cost (actual + projected) contributed by this KPI
    /// card's aggregate. Present only on the "Total cost (list-price)" card.
    #[serde(skip_serializing_if = "Option::is_none")]
    list_price_usd: Option<f64>,
    /// Unpriced session count contributed by this KPI card's aggregate.
    /// Present only on the "Sessions" card.
    #[serde(skip_serializing_if = "Option::is_none")]
    unpriced_count: Option<i64>,
}

/// One labelled component of a KPI's headline value (see
/// [`UsageKpiDto::breakdown`]). The UI formats `value` as a compact number.
#[derive(Serialize, JsonSchema)]
struct UsageKpiPartDto {
    label: String,
    value: f64,
}

/// A point in the multi-dimensional time series.  Carries the model / project /
/// agent dimensions so the Overview tab can group spend client-side.
#[derive(Serialize, JsonSchema)]
struct SeriesPointDto {
    date: String,
    /// Actual API spend in USD for this bucket; `null` when no actual sessions.
    actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost; `null` when no projected sessions.
    projected_usd: Option<f64>,
    /// Combined list-price cost (actual + projected); `null` when no priced
    /// sessions exist in this bucket.
    list_price_usd: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    tokens_cached: i64,
    task_count: i64,
    model: String,
    project_id: String,
    project_name: String,
    agent_type: String,
    /// Count of unpriced sessions in this bucket.
    unpriced_session_count: i64,
}

/// A breakdown row for one entity (user / project / proposal / task).
#[derive(Serialize, JsonSchema)]
struct BreakdownRowDto {
    id: String,
    name: String,
    actual_spend_usd: Option<f64>,
    projected_usd: Option<f64>,
    /// Combined list-price cost (actual + projected). `None` when no priced
    /// sessions exist for this entity.
    list_price_usd: Option<f64>,
    unpriced_session_count: i64,
    tokens_in: i64,
    tokens_out: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    tokens_cached: i64,
    task_count: i64,
    success_rate: Option<f64>,
    avg_reopens: Option<f64>,
    /// Actual API spend per task. `None` when no actual-basis sessions.
    actual_cost_per_task: Option<f64>,
    /// Combined list-price cost per task (actual + projected). `None` when no
    /// priced sessions or no tasks.
    list_price_cost_per_task: Option<f64>,
    /// Present only for the by_task breakdown; lets the UI build a task link.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// Present only for the by_proposal breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    proposal_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
struct BreakdownsDto {
    by_user: Vec<BreakdownRowDto>,
    by_project: Vec<BreakdownRowDto>,
    by_proposal: Vec<BreakdownRowDto>,
    by_task: Vec<BreakdownRowDto>,
}

/// Per-model effectiveness, renamed to the frontend contract.
#[derive(Serialize, JsonSchema)]
struct ModelEffectivenessDto {
    model: String,
    task_count: i64,
    success_rate: Option<f64>,
    avg_reopens: Option<f64>,
    /// Actual API spend per completed task.
    actual_cost_per_task: Option<f64>,
    /// Combined list-price cost per completed task (actual + projected). This
    /// is the primary, apples-to-apples "Cost / task" figure in the UI.
    list_price_cost_per_task: Option<f64>,
    /// Actual API spend total.
    actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost total.
    projected_usd: Option<f64>,
    /// Combined list-price cost total (actual + projected).
    list_price_usd: Option<f64>,
    /// Sessions excluded from both dollar figures.
    unpriced_session_count: i64,
    total_tokens: i64,
    tokens_in: i64,
    tokens_out: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    tokens_cached: i64,
    session_count: i64,
    /// Shared-credit completed-task count (== task_count); surfaced separately
    /// so the UI can label the attribution semantics.
    completed_task_count: i64,
    /// First-pass rejection rate (0.0–1.0): fraction of this model's worker
    /// sessions that were superseded by a later worker session on a reopened
    /// task — the pass did not land and the task was reworked. Discriminates
    /// first-pass quality that shared-credit success_rate hides. `None` when
    /// the model ran no worker sessions.
    first_pass_rejection_rate: Option<f64>,
    /// Final-pass share (0.0–1.0): fraction of this model's shared-credit
    /// completed tasks where THIS model ran the last worker session before the
    /// task closed — i.e. who actually landed the merge. `None` when the model
    /// has no completed-task credits.
    final_pass_share: Option<f64>,
    /// Worker sessions superseded on a reopened task (numerator of
    /// `first_pass_rejection_rate`).
    first_pass_rejected_session_count: i64,
    /// Completed tasks landed by this model's final worker session (numerator of
    /// `final_pass_share`).
    final_pass_completed_task_count: i64,
}

impl From<ModelEffectivenessRow> for ModelEffectivenessDto {
    fn from(r: ModelEffectivenessRow) -> Self {
        Self {
            model: r.model_id,
            task_count: r.shared_credit_completed_task_count,
            success_rate: r.success_rate,
            avg_reopens: r.avg_reopens,
            actual_cost_per_task: r.actual_cost_per_completed_task,
            list_price_cost_per_task: r.list_price_cost_per_completed_task,
            actual_spend_usd: r.actual_spend_usd,
            projected_usd: r.projected_usd,
            list_price_usd: r.list_price_usd,
            unpriced_session_count: r.unpriced_session_count,
            total_tokens: r.tokens_in + r.tokens_out,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            tokens_cached: r.cache_read_tokens,
            session_count: r.sessions,
            completed_task_count: r.shared_credit_completed_task_count,
            first_pass_rejection_rate: r.first_pass_rejection_rate,
            final_pass_share: r.final_pass_share,
            first_pass_rejected_session_count: r.first_pass_rejected_session_count,
            final_pass_completed_task_count: r.final_pass_completed_task_count,
        }
    }
}

/// Project × model matrix cell, renamed to the frontend contract.
#[derive(Serialize, JsonSchema)]
struct ProjectModelCellDto {
    project_id: String,
    project_name: String,
    model: String,
    /// Actual API spend per task.
    actual_cost_per_task: Option<f64>,
    /// Combined list-price cost per task (actual + projected). This is the
    /// primary, apples-to-apples "Cost / task" figure in the UI.
    list_price_cost_per_task: Option<f64>,
    success_rate: Option<f64>,
    avg_reopens: Option<f64>,
    /// Actual API spend total.
    actual_spend_usd: Option<f64>,
    /// Projected subscription-equivalent cost total.
    projected_usd: Option<f64>,
    /// Combined list-price cost total (actual + projected).
    list_price_usd: Option<f64>,
    /// Sessions excluded from both dollar figures.
    unpriced_session_count: i64,
    total_tokens: i64,
    /// Cache-read (cached input) tokens — priced separately from fresh input.
    tokens_cached: i64,
}

impl From<ProjectModelMatrixRow> for ProjectModelCellDto {
    fn from(r: ProjectModelMatrixRow) -> Self {
        // Use actual spend for cost-per-task to reflect real API spend.
        let actual_cost_per_task = match (r.actual_spend_usd, r.task_count) {
            (Some(cost), n) if n > 0 => Some(cost / n as f64),
            _ => None,
        };
        // Combined list-price cost per task (actual + projected) — the
        // apples-to-apples axis across API-key and flat-rate-plan models.
        let list_price_cost_per_task = match (r.list_price_usd, r.task_count) {
            (Some(cost), n) if n > 0 => Some(cost / n as f64),
            _ => None,
        };
        Self {
            project_id: r.project_id,
            project_name: r.project_name,
            model: r.model_id,
            actual_cost_per_task,
            list_price_cost_per_task,
            success_rate: r.success_rate,
            avg_reopens: r.avg_reopens,
            actual_spend_usd: r.actual_spend_usd,
            projected_usd: r.projected_usd,
            list_price_usd: r.list_price_usd,
            unpriced_session_count: r.unpriced_session_count,
            total_tokens: r.tokens_in + r.tokens_out,
            tokens_cached: r.cache_read_tokens,
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct UsageResponse {
    kpis: Vec<UsageKpiDto>,
    time_series: Vec<SeriesPointDto>,
    breakdowns: BreakdownsDto,
    model_effectiveness: Vec<ModelEffectivenessDto>,
    project_model_matrix: Vec<ProjectModelCellDto>,
    /// RFC3339 timestamp when the response was generated.
    generated_at: String,
    /// Total number of unpriced sessions in the current window, repeated at
    /// top-level for convenience. The frontend reducer falls back to this when
    /// no KPI card carries `unpriced_count`.
    unpriced_session_count: i64,
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

/// Combine actual and projected dollar figures into a single list-price total.
/// Each side is treated as 0 when present-but-null on the other, but the result
/// stays `None` when both sides are absent (mirrors the SQL
/// `SUM(...) FILTER (WHERE cost_basis IN ('actual','projected'))` NULL-safety).
fn combine_list_price(actual: Option<f64>, projected: Option<f64>) -> Option<f64> {
    match (actual, projected) {
        (None, None) => None,
        (a, p) => Some(a.unwrap_or(0.0) + p.unwrap_or(0.0)),
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
    let actual_delta = match (totals.actual_spend_usd, previous.actual_spend_usd) {
        (Some(cur), Some(prev)) => pct_delta(cur, prev),
        _ => None,
    };

    let projected_delta = match (totals.projected_usd, previous.projected_usd) {
        (Some(cur), Some(prev)) => pct_delta(cur, prev),
        _ => None,
    };

    let list_price_delta = match (totals.list_price_usd, previous.list_price_usd) {
        (Some(cur), Some(prev)) => pct_delta(cur, prev),
        _ => None,
    };

    // The real API spend that is a subset of the combined list-price figure,
    // surfaced in the primary card's caption.
    let list_price_caption = {
        let real = totals.actual_spend_usd.unwrap_or(0.0);
        cost_caption(
            totals.unpriced_session_count,
            &format!(
                "Actual + projected at catalog list rates · {} real API spend",
                format_currency(real)
            ),
        )
    };

    let tokens = (totals.tokens_in + totals.tokens_out) as f64;
    let prev_tokens = (previous.tokens_in + previous.tokens_out) as f64;

    vec![
        UsageKpiDto {
            label: "Total cost (list-price)".to_string(),
            value: totals.list_price_usd,
            delta_pct: list_price_delta,
            formatted: totals
                .list_price_usd
                .map(format_currency)
                .unwrap_or_default(),
            caption: Some(list_price_caption),
            breakdown: None,
            actual_spend_usd: None,
            projected_usd: None,
            list_price_usd: totals.list_price_usd,
            unpriced_count: None,
        },
        UsageKpiDto {
            label: "Actual API Spend".to_string(),
            value: totals.actual_spend_usd,
            delta_pct: actual_delta,
            formatted: totals
                .actual_spend_usd
                .map(format_currency)
                .unwrap_or_default(),
            caption: Some(cost_caption(
                totals.unpriced_session_count,
                "Real API-key spend at list rates",
            )),
            breakdown: None,
            actual_spend_usd: totals.actual_spend_usd,
            projected_usd: None,
            list_price_usd: None,
            unpriced_count: None,
        },
        UsageKpiDto {
            label: "Projected Cost".to_string(),
            value: totals.projected_usd,
            delta_pct: projected_delta,
            formatted: totals
                .projected_usd
                .map(format_currency)
                .unwrap_or_default(),
            caption: Some("Subscription / coding-plan list-rate equivalent".to_string()),
            breakdown: None,
            actual_spend_usd: None,
            projected_usd: totals.projected_usd,
            list_price_usd: None,
            unpriced_count: None,
        },
        UsageKpiDto {
            label: "Tokens".to_string(),
            value: Some(tokens),
            delta_pct: pct_delta(tokens, prev_tokens),
            formatted: String::new(),
            caption: None,
            // Split the headline token count into the three meaningful buckets:
            // fresh input, cache-read (cached input), and output. Cache-read is
            // tracked separately from `tokens_in`, so this is purely additive
            // context, not a re-slice of the `tokens` total.
            breakdown: Some(vec![
                UsageKpiPartDto {
                    label: "Input".to_string(),
                    value: totals.tokens_in as f64,
                },
                UsageKpiPartDto {
                    label: "Cached".to_string(),
                    value: totals.cache_read_tokens as f64,
                },
                UsageKpiPartDto {
                    label: "Output".to_string(),
                    value: totals.tokens_out as f64,
                },
            ]),
            actual_spend_usd: None,
            projected_usd: None,
            list_price_usd: None,
            unpriced_count: None,
        },
        UsageKpiDto {
            label: "Sessions".to_string(),
            value: Some(totals.session_count as f64),
            delta_pct: pct_delta(totals.session_count as f64, previous.session_count as f64),
            formatted: String::new(),
            caption: None,
            breakdown: None,
            actual_spend_usd: None,
            projected_usd: None,
            list_price_usd: None,
            unpriced_count: Some(totals.unpriced_session_count),
        },
        UsageKpiDto {
            label: "Cache reads".to_string(),
            value: Some(totals.cache_read_tokens as f64),
            delta_pct: pct_delta(
                totals.cache_read_tokens as f64,
                previous.cache_read_tokens as f64,
            ),
            formatted: String::new(),
            caption: None,
            breakdown: None,
            actual_spend_usd: None,
            projected_usd: None,
            list_price_usd: None,
            unpriced_count: None,
        },
    ]
}

/// Caption for cost KPI cards that notes unpriced session exclusion.
fn cost_caption(unpriced_session_count: i64, base: &str) -> String {
    let mut caption = base.to_string();
    if unpriced_session_count > 0 {
        caption.push_str(&format!(
            " · {unpriced_session_count} unpriced session{} excluded",
            if unpriced_session_count == 1 { "" } else { "s" }
        ));
    }
    caption
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

/// Accumulates actual spend, projected cost, and unpriced counts for a
/// rolled-up series bucket.
#[derive(Default)]
struct SeriesAccumulator {
    tokens_in: i64,
    tokens_out: i64,
    tokens_cached: i64,
    task_count: i64,
    actual_spend_sum: Option<f64>,
    projected_sum: Option<f64>,
    unpriced_session_count: i64,
}

impl SeriesAccumulator {
    fn add(&mut self, row: &SeriesDetailRow) {
        self.tokens_in += row.tokens_in;
        self.tokens_out += row.tokens_out;
        self.tokens_cached += row.cache_read_tokens;
        self.task_count += row.task_count;
        self.unpriced_session_count += row.unpriced_session_count;

        if let Some(actual) = row.actual_spend_usd {
            *self.actual_spend_sum.get_or_insert(0.0) += actual;
        }
        if let Some(proj) = row.projected_usd {
            *self.projected_sum.get_or_insert(0.0) += proj;
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
        .map(
            |((date, model, project_id, project_name, agent_type), acc)| SeriesPointDto {
                date,
                actual_spend_usd: acc.actual_spend_sum,
                projected_usd: acc.projected_sum,
                list_price_usd: combine_list_price(acc.actual_spend_sum, acc.projected_sum),
                tokens_in: acc.tokens_in,
                tokens_out: acc.tokens_out,
                tokens_cached: acc.tokens_cached,
                task_count: acc.task_count,
                model,
                project_id,
                project_name,
                agent_type,
                unpriced_session_count: acc.unpriced_session_count,
            },
        )
        .collect())
}

/// Map a repository breakdown row to the response DTO, deriving cost-per-task
/// and attaching the entity-specific link id.
fn breakdown_row(row: EntityBreakdownRow, dimension: GroupDimension) -> BreakdownRowDto {
    // Use actual spend for cost-per-task to reflect real API spend.
    let actual_cost_per_task = match (row.actual_spend_usd, row.task_count) {
        (Some(cost), n) if n > 0 => Some(cost / n as f64),
        _ => None,
    };
    // Combined list-price cost per task (actual + projected).
    let list_price_cost_per_task = match (row.list_price_usd, row.task_count) {
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
        actual_spend_usd: row.actual_spend_usd,
        projected_usd: row.projected_usd,
        list_price_usd: row.list_price_usd,
        unpriced_session_count: row.unpriced_session_count,
        tokens_in: row.tokens_in,
        tokens_out: row.tokens_out,
        tokens_cached: row.cache_read_tokens,
        task_count: row.task_count,
        success_rate: row.success_rate,
        avg_reopens: row.avg_reopens,
        actual_cost_per_task,
        list_price_cost_per_task,
        task_id,
        proposal_id,
    }
}

fn breakdown_rows(
    rows: Vec<EntityBreakdownRow>,
    dimension: GroupDimension,
) -> Vec<BreakdownRowDto> {
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
        unpriced_session_count: totals.unpriced_session_count,
    };

    Ok(Json(response))
}

// ── Schema export ────────────────────────────────────────────────────────────

/// Returns the JSON Schema for the `/api/admin/usage` response DTOs.
///
/// Used by `scripts/export-usage-schema` and the `ui/scripts/generate-usage-types.ts`
/// pipeline to produce the checked-in TypeScript contract artifact
/// `ui/src/api/generated/usage-analytics.gen.ts`.  Prefer running
/// `pnpm usage:types` from the `ui/` directory to regenerate.
pub fn usage_response_json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(UsageResponse))
        .expect("UsageResponse schema is always valid JSON")
}

/// Returns the JSON Schema for the `/api/admin/usage` query/filter parameters.
pub fn usage_query_json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(UsageQuery))
        .expect("UsageQuery schema is always valid JSON")
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "usage_analytics_schema_tests.rs"]
mod usage_analytics_schema_tests;

#[cfg(test)]
#[path = "usage_analytics_handler_tests.rs"]
mod tests;
