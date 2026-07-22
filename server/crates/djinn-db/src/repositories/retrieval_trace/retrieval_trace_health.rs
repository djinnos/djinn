//! Health-rollup DTOs, aggregate row mappings, and SQL for retrieval traces.
//!
//! Kept beside `retrieval_trace` as a child module so the repository API remains
//! small enough for the server source-size guard.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::RetrievalTraceEntryPoint;

// ── Health rollup result types ────────────────────────────────────────────────

/// Summary of candidate confidence scores across one or more traces.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateScoreSummary {
    pub count: i64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub sum: Option<f64>,
    pub avg: Option<f64>,
}

#[derive(sqlx::FromRow)]
pub(super) struct TaxonomyV1HealthGroupRow {
    pub(super) project_id: String,
    pub(super) entry_point: String,
    total_queries: i64,
    successful_queries: i64,
    errored_queries: i64,
    zero_candidate_queries: i64,
    candidate_bearing_queries: i64,
    starved_queries: i64,
    injected_queries: i64,
    candidate_total: i64,
    injected_total: i64,
    confidence_filtered_total: i64,
    not_top_k_total: i64,
    oversized_skipped_total: i64,
    injected_disposition_total: i64,
    budget_pruned_total: i64,
    legacy_unclassified_queries: i64,
    invalid_taxonomy_queries: i64,
    validation_errors: serde_json::Value,
}

pub(super) fn build_taxonomy_v1_group(
    row: TaxonomyV1HealthGroupRow,
    window_start: &str,
    window_end: &str,
    refreshed_at: &str,
) -> Result<TaxonomyV1RetrievalHealthGroup, String> {
    let entry_point = RetrievalTraceEntryPoint::parse(&row.entry_point).ok_or_else(|| {
        format!(
            "unknown entry_point in taxonomy-v1 health rollup: {}",
            row.entry_point
        )
    })?;
    let validation_errors = serde_json::from_value(row.validation_errors)
        .map_err(|err| format!("invalid taxonomy-v1 validation telemetry: {err}"))?;
    let counts = TaxonomyV1RetrievalHealthCounts {
        total_queries: row.total_queries,
        successful_queries: row.successful_queries,
        errored_queries: row.errored_queries,
        zero_candidate_queries: row.zero_candidate_queries,
        candidate_bearing_queries: row.candidate_bearing_queries,
        starved_queries: row.starved_queries,
        injected_queries: row.injected_queries,
        candidate_total: row.candidate_total,
        injected_total: row.injected_total,
        confidence_filtered_total: row.confidence_filtered_total,
        not_top_k_total: row.not_top_k_total,
        oversized_skipped_total: row.oversized_skipped_total,
        injected_disposition_total: row.injected_disposition_total,
        budget_pruned_total: row.budget_pruned_total,
        legacy_unclassified_queries: row.legacy_unclassified_queries,
        invalid_taxonomy_queries: row.invalid_taxonomy_queries,
    };
    Ok(TaxonomyV1RetrievalHealthGroup {
        project_id: row.project_id,
        entry_point,
        taxonomy_version: 1,
        window_start: window_start.to_owned(),
        window_end: window_end.to_owned(),
        refreshed_at: refreshed_at.to_owned(),
        invalid: counts.invalid_taxonomy_queries > 0,
        counts,
        validation_errors,
    })
}

/// Execute the sole authoritative terminal-time bounded health query.
/// Legacy/unknown rows and malformed v1 rows are classified before aggregation
/// and cannot affect versioned counters or histograms. `terminal_at` is stored
/// as text, so compare its parsed PostgreSQL timestamp value rather than its
/// RFC3339 spelling; this preserves the exact `[from, until)` boundary even
/// when persisted fractional-second precision differs from the query bound.
pub(super) async fn fetch_taxonomy_v1_health_groups(
    pool: &PgPool,
    from: &str,
    until: &str,
) -> std::result::Result<Vec<TaxonomyV1HealthGroupRow>, sqlx::Error> {
    sqlx::query_as!(
        TaxonomyV1HealthGroupRow,
        r#"
WITH bounded AS (
 SELECT id, project_id, entry_point, knowledge_trace_taxonomy_version, terminal_state,
        candidate_count, injected_count, confidence_filtered_count, not_top_k_count,
        oversized_skipped_count, budget_pruned_count FROM retrieval_traces
 WHERE terminal_at::timestamptz >= $1::text::timestamptz
   AND terminal_at::timestamptz < $2::text::timestamptz
   AND entry_point IN ('dispatch','jit_pitfalls','load_knowledge_context','format_knowledge_notes')
), classified AS (
 SELECT *, CASE
  WHEN knowledge_trace_taxonomy_version IS DISTINCT FROM 1 THEN 'legacy'
  WHEN terminal_state = 'success' AND candidate_count IS NOT NULL AND injected_count IS NOT NULL
   AND confidence_filtered_count IS NOT NULL AND not_top_k_count IS NOT NULL AND oversized_skipped_count IS NOT NULL AND budget_pruned_count IS NOT NULL
   AND candidate_count >= 0 AND injected_count >= 0 AND confidence_filtered_count >= 0 AND not_top_k_count >= 0 AND oversized_skipped_count >= 0 AND budget_pruned_count >= 0
   AND injected_count::bigint = candidate_count::bigint - confidence_filtered_count::bigint - not_top_k_count::bigint - oversized_skipped_count::bigint - budget_pruned_count::bigint
   AND candidate_count::bigint = confidence_filtered_count::bigint + not_top_k_count::bigint + oversized_skipped_count::bigint + injected_count::bigint + budget_pruned_count::bigint THEN 'success'
  WHEN terminal_state IN ('error','cancelled') AND candidate_count IS NULL AND injected_count IS NULL AND confidence_filtered_count IS NULL AND not_top_k_count IS NULL AND oversized_skipped_count IS NULL AND budget_pruned_count IS NULL THEN 'exceptional'
  WHEN terminal_state IS NULL OR terminal_state NOT IN ('success','error','cancelled') THEN 'invalid_terminal_state'
  WHEN terminal_state = 'success' AND (candidate_count IS NULL OR injected_count IS NULL OR confidence_filtered_count IS NULL OR not_top_k_count IS NULL OR oversized_skipped_count IS NULL OR budget_pruned_count IS NULL) THEN 'missing_success_counts'
  WHEN terminal_state = 'success' AND (candidate_count < 0 OR injected_count < 0 OR confidence_filtered_count < 0 OR not_top_k_count < 0 OR oversized_skipped_count < 0 OR budget_pruned_count < 0) THEN 'negative_count'
  WHEN terminal_state = 'success' AND injected_count::bigint <> candidate_count::bigint - confidence_filtered_count::bigint - not_top_k_count::bigint - oversized_skipped_count::bigint - budget_pruned_count::bigint THEN 'injected_count_mismatch'
  WHEN terminal_state = 'success' THEN 'histogram_partition_mismatch'
  ELSE 'exceptional_has_counts' END AS classification FROM bounded
)
SELECT project_id, entry_point,
 count(*) FILTER (WHERE classification IN ('success','exceptional'))::bigint AS "total_queries!",
 count(*) FILTER (WHERE classification='success')::bigint AS "successful_queries!",
 count(*) FILTER (WHERE classification='exceptional')::bigint AS "errored_queries!",
 count(*) FILTER (WHERE classification='success' AND candidate_count=0)::bigint AS "zero_candidate_queries!",
 count(*) FILTER (WHERE classification='success' AND candidate_count>0)::bigint AS "candidate_bearing_queries!",
 count(*) FILTER (WHERE classification='success' AND candidate_count>0 AND injected_count=0)::bigint AS "starved_queries!",
 count(*) FILTER (WHERE classification='success' AND injected_count>0)::bigint AS "injected_queries!",
 coalesce(sum(candidate_count) FILTER (WHERE classification='success'),0)::bigint AS "candidate_total!",
 coalesce(sum(injected_count) FILTER (WHERE classification='success'),0)::bigint AS "injected_total!",
 coalesce(sum(confidence_filtered_count) FILTER (WHERE classification='success'),0)::bigint AS "confidence_filtered_total!",
 coalesce(sum(not_top_k_count) FILTER (WHERE classification='success'),0)::bigint AS "not_top_k_total!",
 coalesce(sum(oversized_skipped_count) FILTER (WHERE classification='success'),0)::bigint AS "oversized_skipped_total!",
 coalesce(sum(injected_count) FILTER (WHERE classification='success'),0)::bigint AS "injected_disposition_total!",
 coalesce(sum(budget_pruned_count) FILTER (WHERE classification='success'),0)::bigint AS "budget_pruned_total!",
 count(*) FILTER (WHERE classification='legacy')::bigint AS "legacy_unclassified_queries!",
 count(*) FILTER (WHERE classification NOT IN ('legacy','success','exceptional'))::bigint AS "invalid_taxonomy_queries!",
 coalesce(jsonb_agg(jsonb_build_object('trace_id',id,'reason',classification) ORDER BY id) FILTER (WHERE classification NOT IN ('legacy','success','exceptional')),'[]'::jsonb) AS validation_errors
FROM classified GROUP BY project_id, entry_point ORDER BY project_id, entry_point
"#,
        from,
        until,
    )
    .fetch_all(pool)
    .await
}

/// Summary of a single duration stage (e.g. `retrieval_ms`) across one or more
/// traces.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurationStageSummary {
    pub stage_name: String,
    pub count: i64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub sum: Option<f64>,
    pub avg: Option<f64>,
}

/// Counts of skipped candidates by reason.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SkipReasonCounts {
    pub not_top_k: i64,
    pub min_confidence: i64,
    pub budget_pruned: i64,
    pub superseded_pruned: i64,
    pub dedupe: i64,
    pub search_error: i64,
}

/// Aggregate health evidence for a single scope (combined project or one entry
/// point).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalTraceHealthEvidence {
    pub trace_count: i64,
    /// Traces which produced no injected candidate. This is deliberately a
    /// trace-level count rather than derived from the aggregate injected
    /// candidate count: one trace can inject multiple candidates.
    pub zero_result_trace_count: i64,
    pub candidate_count: i64,
    pub injected_count: i64,
    pub skipped_count: i64,
    pub skip_reason_counts: SkipReasonCounts,
    pub candidate_score_summary: CandidateScoreSummary,
    pub duration_stage_summaries: Vec<DurationStageSummary>,
    pub cap_exceeded_count: i64,
    pub estimated_injected_tokens_sum: i64,
    pub estimated_injected_tokens_avg: Option<f64>,
}

/// Health rollup result for a project over a half-open time window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalTraceHealthRollup {
    pub combined: RetrievalTraceHealthEvidence,
    pub per_entry_point: HashMap<RetrievalTraceEntryPoint, RetrievalTraceHealthEvidence>,
}

/// A deterministic, field-level explanation for a malformed v1 terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalTaxonomyValidationError {
    pub trace_id: String,
    pub reason: String,
}

/// Version-homogeneous counters for one project/entry-point terminal window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyV1RetrievalHealthCounts {
    pub total_queries: i64,
    pub successful_queries: i64,
    pub errored_queries: i64,
    pub zero_candidate_queries: i64,
    pub candidate_bearing_queries: i64,
    pub starved_queries: i64,
    pub injected_queries: i64,
    pub candidate_total: i64,
    pub injected_total: i64,
    pub confidence_filtered_total: i64,
    pub not_top_k_total: i64,
    pub oversized_skipped_total: i64,
    pub injected_disposition_total: i64,
    pub budget_pruned_total: i64,
    pub legacy_unclassified_queries: i64,
    pub invalid_taxonomy_queries: i64,
}

/// Authoritative bounded taxonomy-v1 evidence. The window is `[start, end)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxonomyV1RetrievalHealthGroup {
    pub project_id: String,
    pub entry_point: RetrievalTraceEntryPoint,
    pub taxonomy_version: i32,
    pub window_start: String,
    pub window_end: String,
    pub refreshed_at: String,
    pub invalid: bool,
    pub counts: TaxonomyV1RetrievalHealthCounts,
    pub validation_errors: Vec<RetrievalTaxonomyValidationError>,
}

// ── Health rollup helpers ───────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
pub(super) struct TraceCandidateStatsRow {
    pub(super) entry_point: String,
    trace_count: i64,
    zero_result_trace_count: i64,
    cap_exceeded_count: i64,
    estimated_injected_tokens_sum: i64,
    candidate_count: i64,
    injected_count: i64,
    skipped_count: i64,
    not_top_k_count: i64,
    min_confidence_count: i64,
    budget_pruned_count: i64,
    superseded_pruned_count: i64,
    dedupe_count: i64,
    search_error_count: i64,
    min_confidence: Option<f64>,
    max_confidence: Option<f64>,
    avg_confidence: Option<f64>,
    sum_confidence: Option<f64>,
}

#[derive(sqlx::FromRow)]
pub(super) struct TraceCandidateStatsCombinedRow {
    trace_count: i64,
    zero_result_trace_count: i64,
    cap_exceeded_count: i64,
    estimated_injected_tokens_sum: i64,
    candidate_count: i64,
    injected_count: i64,
    skipped_count: i64,
    not_top_k_count: i64,
    min_confidence_count: i64,
    budget_pruned_count: i64,
    superseded_pruned_count: i64,
    dedupe_count: i64,
    search_error_count: i64,
    min_confidence: Option<f64>,
    max_confidence: Option<f64>,
    avg_confidence: Option<f64>,
    sum_confidence: Option<f64>,
}

#[derive(sqlx::FromRow, Clone)]
pub(super) struct DurationStageStatsRow {
    pub(super) entry_point: String,
    stage_name: String,
    count: i64,
    min_ms: Option<f64>,
    max_ms: Option<f64>,
    avg_ms: Option<f64>,
    sum_ms: Option<f64>,
}

#[derive(sqlx::FromRow, Clone)]
pub(super) struct DurationStageStatsCombinedRow {
    stage_name: String,
    count: i64,
    min_ms: Option<f64>,
    max_ms: Option<f64>,
    avg_ms: Option<f64>,
    sum_ms: Option<f64>,
}

pub(super) fn build_evidence(
    stats: &TraceCandidateStatsRow,
    durations: &[DurationStageStatsRow],
) -> RetrievalTraceHealthEvidence {
    let estimated_injected_tokens_avg = if stats.trace_count > 0 {
        Some(stats.estimated_injected_tokens_sum as f64 / stats.trace_count as f64)
    } else {
        None
    };

    RetrievalTraceHealthEvidence {
        trace_count: stats.trace_count,
        zero_result_trace_count: stats.zero_result_trace_count,
        candidate_count: stats.candidate_count,
        injected_count: stats.injected_count,
        skipped_count: stats.skipped_count,
        skip_reason_counts: SkipReasonCounts {
            not_top_k: stats.not_top_k_count,
            min_confidence: stats.min_confidence_count,
            budget_pruned: stats.budget_pruned_count,
            superseded_pruned: stats.superseded_pruned_count,
            dedupe: stats.dedupe_count,
            search_error: stats.search_error_count,
        },
        candidate_score_summary: CandidateScoreSummary {
            count: stats.candidate_count,
            min: stats.min_confidence,
            max: stats.max_confidence,
            sum: stats.sum_confidence,
            avg: stats.avg_confidence,
        },
        duration_stage_summaries: durations
            .iter()
            .map(|d| DurationStageSummary {
                stage_name: d.stage_name.clone(),
                count: d.count,
                min: d.min_ms,
                max: d.max_ms,
                sum: d.sum_ms,
                avg: d.avg_ms,
            })
            .collect(),
        cap_exceeded_count: stats.cap_exceeded_count,
        estimated_injected_tokens_sum: stats.estimated_injected_tokens_sum,
        estimated_injected_tokens_avg,
    }
}

pub(super) fn build_evidence_combined(
    stats: &TraceCandidateStatsCombinedRow,
    durations: &[DurationStageStatsCombinedRow],
) -> RetrievalTraceHealthEvidence {
    let estimated_injected_tokens_avg = if stats.trace_count > 0 {
        Some(stats.estimated_injected_tokens_sum as f64 / stats.trace_count as f64)
    } else {
        None
    };

    RetrievalTraceHealthEvidence {
        trace_count: stats.trace_count,
        zero_result_trace_count: stats.zero_result_trace_count,
        candidate_count: stats.candidate_count,
        injected_count: stats.injected_count,
        skipped_count: stats.skipped_count,
        skip_reason_counts: SkipReasonCounts {
            not_top_k: stats.not_top_k_count,
            min_confidence: stats.min_confidence_count,
            budget_pruned: stats.budget_pruned_count,
            superseded_pruned: stats.superseded_pruned_count,
            dedupe: stats.dedupe_count,
            search_error: stats.search_error_count,
        },
        candidate_score_summary: CandidateScoreSummary {
            count: stats.candidate_count,
            min: stats.min_confidence,
            max: stats.max_confidence,
            sum: stats.sum_confidence,
            avg: stats.avg_confidence,
        },
        duration_stage_summaries: durations
            .iter()
            .map(|d| DurationStageSummary {
                stage_name: d.stage_name.clone(),
                count: d.count,
                min: d.min_ms,
                max: d.max_ms,
                sum: d.sum_ms,
                avg: d.avg_ms,
            })
            .collect(),
        cap_exceeded_count: stats.cap_exceeded_count,
        estimated_injected_tokens_sum: stats.estimated_injected_tokens_sum,
        estimated_injected_tokens_avg,
    }
}

pub(super) const HEALTH_ROLLUP_TRACE_CANDIDATE_PER_EP_SQL: &str = r#"
    WITH filtered AS (
        SELECT
            id,
            entry_point,
            candidate_cap_exceeded,
            estimated_injected_tokens,
            candidates
        FROM retrieval_traces
        WHERE project_id = $1
          AND created_at >= $2
          AND created_at < $3
          AND entry_point IN ('dispatch', 'jit_pitfalls', 'load_knowledge_context', 'format_knowledge_notes')
    ),
    trace_stats AS (
        SELECT
            entry_point,
            count(*)::bigint AS trace_count,
            count(*) FILTER (
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM jsonb_array_elements(
                        CASE WHEN jsonb_typeof(candidates) = 'array' THEN candidates ELSE '[]'::jsonb END
                    ) candidate
                    WHERE candidate->>'outcome' = 'injected'
                )
            )::bigint AS zero_result_trace_count,
            coalesce(sum((candidate_cap_exceeded)::int), 0)::bigint AS cap_exceeded_count,
            coalesce(sum(estimated_injected_tokens), 0)::bigint AS estimated_injected_tokens_sum
        FROM filtered
        GROUP BY entry_point
    ),
    candidate_stats AS (
        SELECT
            f.entry_point,
            count(c.value)::bigint AS candidate_count,
            count(*) FILTER (WHERE c.value->>'outcome' = 'injected')::bigint AS injected_count,
            count(*) FILTER (WHERE c.value->>'outcome' = 'skipped')::bigint AS skipped_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'not_top_k')::bigint AS not_top_k_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'min_confidence')::bigint AS min_confidence_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'budget_pruned')::bigint AS budget_pruned_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'superseded_pruned')::bigint AS superseded_pruned_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'dedupe')::bigint AS dedupe_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'search_error')::bigint AS search_error_count,
            min((c.value->>'confidence')::double precision) AS min_confidence,
            max((c.value->>'confidence')::double precision) AS max_confidence,
            avg((c.value->>'confidence')::double precision) AS avg_confidence,
            sum((c.value->>'confidence')::double precision) AS sum_confidence
        FROM filtered f
        LEFT JOIN LATERAL jsonb_array_elements(
            CASE WHEN jsonb_typeof(f.candidates) = 'array' THEN f.candidates ELSE '[]'::jsonb END
        ) c ON true
        GROUP BY f.entry_point
    )
    SELECT
        ts.entry_point,
        ts.trace_count,
        ts.zero_result_trace_count,
        ts.cap_exceeded_count,
        ts.estimated_injected_tokens_sum,
        cs.candidate_count,
        cs.injected_count,
        cs.skipped_count,
        cs.not_top_k_count,
        cs.min_confidence_count,
        cs.budget_pruned_count,
        cs.superseded_pruned_count,
        cs.dedupe_count,
        cs.search_error_count,
        cs.min_confidence,
        cs.max_confidence,
        cs.avg_confidence,
        cs.sum_confidence
    FROM trace_stats ts
    LEFT JOIN candidate_stats cs ON ts.entry_point = cs.entry_point
"#;

pub(super) const HEALTH_ROLLUP_TRACE_CANDIDATE_COMBINED_SQL: &str = r#"
    WITH filtered AS (
        SELECT
            id,
            candidate_cap_exceeded,
            estimated_injected_tokens,
            candidates
        FROM retrieval_traces
        WHERE project_id = $1
          AND created_at >= $2
          AND created_at < $3
          AND entry_point IN ('dispatch', 'jit_pitfalls', 'load_knowledge_context', 'format_knowledge_notes')
    ),
    trace_stats AS (
        SELECT
            count(*)::bigint AS trace_count,
            count(*) FILTER (
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM jsonb_array_elements(
                        CASE WHEN jsonb_typeof(candidates) = 'array' THEN candidates ELSE '[]'::jsonb END
                    ) candidate
                    WHERE candidate->>'outcome' = 'injected'
                )
            )::bigint AS zero_result_trace_count,
            coalesce(sum((candidate_cap_exceeded)::int), 0)::bigint AS cap_exceeded_count,
            coalesce(sum(estimated_injected_tokens), 0)::bigint AS estimated_injected_tokens_sum
        FROM filtered
    ),
    candidate_stats AS (
        SELECT
            count(c.value)::bigint AS candidate_count,
            count(*) FILTER (WHERE c.value->>'outcome' = 'injected')::bigint AS injected_count,
            count(*) FILTER (WHERE c.value->>'outcome' = 'skipped')::bigint AS skipped_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'not_top_k')::bigint AS not_top_k_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'min_confidence')::bigint AS min_confidence_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'budget_pruned')::bigint AS budget_pruned_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'superseded_pruned')::bigint AS superseded_pruned_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'dedupe')::bigint AS dedupe_count,
            count(*) FILTER (WHERE c.value->>'skipped_reason' = 'search_error')::bigint AS search_error_count,
            min((c.value->>'confidence')::double precision) AS min_confidence,
            max((c.value->>'confidence')::double precision) AS max_confidence,
            avg((c.value->>'confidence')::double precision) AS avg_confidence,
            sum((c.value->>'confidence')::double precision) AS sum_confidence
        FROM filtered f
        LEFT JOIN LATERAL jsonb_array_elements(
            CASE WHEN jsonb_typeof(f.candidates) = 'array' THEN f.candidates ELSE '[]'::jsonb END
        ) c ON true
    )
    SELECT
        ts.trace_count,
        ts.zero_result_trace_count,
        ts.cap_exceeded_count,
        ts.estimated_injected_tokens_sum,
        cs.candidate_count,
        cs.injected_count,
        cs.skipped_count,
        cs.not_top_k_count,
        cs.min_confidence_count,
        cs.budget_pruned_count,
        cs.superseded_pruned_count,
        cs.dedupe_count,
        cs.search_error_count,
        cs.min_confidence,
        cs.max_confidence,
        cs.avg_confidence,
        cs.sum_confidence
    FROM trace_stats ts
    CROSS JOIN candidate_stats cs
"#;

pub(super) const HEALTH_ROLLUP_DURATION_PER_EP_SQL: &str = r#"
    SELECT
        f.entry_point,
        d.key AS stage_name,
        count(*)::bigint AS count,
        min(d.value::double precision) AS min_ms,
        max(d.value::double precision) AS max_ms,
        avg(d.value::double precision) AS avg_ms,
        sum(d.value::double precision) AS sum_ms
    FROM retrieval_traces f
    CROSS JOIN LATERAL jsonb_each(
        CASE WHEN jsonb_typeof(f.durations_ms) = 'object' THEN f.durations_ms ELSE '{}'::jsonb END
    ) d
    WHERE f.project_id = $1
      AND f.created_at >= $2
      AND f.created_at < $3
      AND f.entry_point IN ('dispatch', 'jit_pitfalls', 'load_knowledge_context', 'format_knowledge_notes')
      AND jsonb_typeof(d.value) = 'number'
    GROUP BY f.entry_point, d.key
"#;

pub(super) const HEALTH_ROLLUP_DURATION_COMBINED_SQL: &str = r#"
    SELECT
        d.key AS stage_name,
        count(*)::bigint AS count,
        min(d.value::double precision) AS min_ms,
        max(d.value::double precision) AS max_ms,
        avg(d.value::double precision) AS avg_ms,
        sum(d.value::double precision) AS sum_ms
    FROM retrieval_traces f
    CROSS JOIN LATERAL jsonb_each(
        CASE WHEN jsonb_typeof(f.durations_ms) = 'object' THEN f.durations_ms ELSE '{}'::jsonb END
    ) d
    WHERE f.project_id = $1
      AND f.created_at >= $2
      AND f.created_at < $3
      AND f.entry_point IN ('dispatch', 'jit_pitfalls', 'load_knowledge_context', 'format_knowledge_notes')
      AND jsonb_typeof(d.value) = 'number'
    GROUP BY d.key
"#;
