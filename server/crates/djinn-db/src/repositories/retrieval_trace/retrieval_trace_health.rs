//! Health-rollup DTOs, aggregate row mappings, and SQL for retrieval traces.
//!
//! Kept beside `retrieval_trace` as a child module so the repository API remains
//! small enough for the server source-size guard.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
