//! Rank metric computation and compare-policy evaluation.
//!
//! Computes recall@k, MRR, zero-result rate, and directional precision/F1.
//! The compare policy gates PR merges against committed baselines.
//!
//! # Gating metrics
//!
//! - **recall\@1, recall\@5, recall\@10**: fraction of queries where at least
//!   one relevant note appears at rank ≤ k.
//! - **MRR** (Mean Reciprocal Rank): mean of 1/best-rank for each query.
//! - **Zero-result rate**: fraction of queries where no relevant note appeared
//!   in the top-k results.
//!
//! # Non-gating / directional metrics
//!
//! - **Precision\@10** and **F1\@10** are computed but clearly marked as
//!   directional/non-gating because mined `tasks.memory_refs` labels are
//!   sparse — they represent a subset of truly relevant notes, so true
//!   precision and recall relative to all relevant notes are unknowable.
//!
//! # Age-bucket recall curves
//!
//! Recall is broken down by note age to surface over-decay regressions:
//! `<7d`, `7-30d`, `30-90d`, `>90d` (over-decay threshold).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::fixtures::BadCaseType;
use crate::report::QueryRankBaseline;
use crate::run::QueryRankRecord;

// ── Age bucketing ─────────────────────────────────────────────────────────

/// Age bucket classification for recall curve analysis.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgeBucket {
    /// Notes accessed within the last 7 days (fresh).
    Under7d,
    /// Notes accessed 7–30 days ago (recent).
    Days7to30,
    /// Notes accessed 30–90 days ago (mature).
    Days30to90,
    /// Notes accessed >90 days ago (over decay threshold).
    OverDecayThreshold,
}

impl AgeBucket {
    /// Classify a note age in days into an age bucket.
    pub fn from_days(age_days: u32) -> Self {
        if age_days < 7 {
            AgeBucket::Under7d
        } else if age_days < 30 {
            AgeBucket::Days7to30
        } else if age_days < 90 {
            AgeBucket::Days30to90
        } else {
            AgeBucket::OverDecayThreshold
        }
    }

    /// Canonical ordering of age buckets for display.
    pub fn all() -> &'static [AgeBucket] {
        &[
            AgeBucket::Under7d,
            AgeBucket::Days7to30,
            AgeBucket::Days30to90,
            AgeBucket::OverDecayThreshold,
        ]
    }
}

impl std::fmt::Display for AgeBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgeBucket::Under7d => write!(f, "<7d"),
            AgeBucket::Days7to30 => write!(f, "7-30d"),
            AgeBucket::Days30to90 => write!(f, "30-90d"),
            AgeBucket::OverDecayThreshold => write!(f, ">90d"),
        }
    }
}

// ── Per-query result record (with age data) ───────────────────────────────

/// A single query result record with age data for report generation.
///
/// Produced during metric computation and stored in report JSON so that
/// the `compare` command can access per-query details without re-running
/// the benchmark.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryResultRecord {
    pub query_id: String,
    pub query_text: String,
    pub expected_permalinks: Vec<String>,
    pub result_permalinks: Vec<String>,
    /// Best (lowest) rank of any expected relevant note, or None if absent.
    pub best_rank: Option<usize>,
    /// Per expected permalink: the 1-based rank, or None if not found.
    pub relevant_ranks: Vec<Option<usize>>,
    pub is_bad_case: bool,
    pub bad_case_type: Option<BadCaseType>,
    /// Age in days for each expected relevant note (derived from corpus note
    /// `last_accessed` timestamp). Same indexing as `expected_permalinks`.
    pub note_ages_days: Vec<u32>,
}

/// Recall at k=1,5,10 for a specific subset.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecallAtK {
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
}

// ── Suite-level gating metrics ────────────────────────────────────────────

/// Gating metrics for a suite (set of query results).
///
/// These are the metrics that drive the compare-policy gate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SuiteMetrics {
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
    /// Fraction of queries with zero relevant results in top-k.
    pub zero_result_rate: f64,
    pub query_count: usize,
}

// ── Directional (non-gating) metrics ─────────────────────────────────────

/// Directional metrics (precision/F1) that are **non-gating** because
/// mined `tasks.memory_refs` labels are sparse.
///
/// These appear in reports for informational purposes only.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DirectionalMetrics {
    pub label: String,
    pub avg_precision_at_10: f64,
    pub avg_recall_at_10_directional: f64,
    pub avg_f1_at_10: f64,
    pub query_count: usize,
}

// ── Aggregate metrics ────────────────────────────────────────────────────

/// Aggregate metrics across all non-bad-case query suites.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AggregateMetrics {
    pub recall_at_1: f64,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
    pub zero_result_rate: f64,
    pub query_count: usize,
}

// ── Core metric computations ─────────────────────────────────────────────

/// Recall\@k: fraction of query records where at least one relevant note
/// appears at rank ≤ k.
///
/// This is a **gating** metric.
pub fn recall_at_k(records: &[QueryRankRecord], k: usize) -> f64 {
    if records.is_empty() {
        return 0.0;
    }
    let hits = records
        .iter()
        .filter(|r| {
            r.relevant_ranks
                .iter()
                .any(|rank| matches!(rank, Some(r) if *r <= k))
        })
        .count();
    hits as f64 / records.len() as f64
}

/// Mean Reciprocal Rank (MRR): mean of 1/best-rank for each query.
///
/// The best rank is the minimum (i.e., highest position) rank among all
/// expected relevant notes for the query. If no relevant note is found,
/// the reciprocal is 0.
///
/// This is a **gating** metric.
pub fn mrr(records: &[QueryRankRecord]) -> f64 {
    if records.is_empty() {
        return 0.0;
    }
    let sum_rr: f64 = records
        .iter()
        .map(|r| {
            r.relevant_ranks
                .iter()
                .filter_map(|rank| *rank)
                .min()
                .map(|best| 1.0 / best as f64)
                .unwrap_or(0.0)
        })
        .sum();
    sum_rr / records.len() as f64
}

/// Zero-result rate: fraction of queries where no relevant note appeared
/// in the top-k results.
///
/// This is a **gating** metric.
pub fn zero_result_rate(records: &[QueryRankRecord]) -> f64 {
    if records.is_empty() {
        return 0.0;
    }
    let zero_count = records
        .iter()
        .filter(|r| r.relevant_ranks.iter().all(|rank| rank.is_none()))
        .count();
    zero_count as f64 / records.len() as f64
}

/// Compute directional (non-gating) precision/F1 metrics.
///
/// Because `tasks.memory_refs` labels are sparse — they represent only a
/// subset of truly relevant notes — these metrics are **directional only**
/// and must never gate PR merges.
pub fn directional_metrics(records: &[QueryRankRecord]) -> DirectionalMetrics {
    if records.is_empty() {
        return DirectionalMetrics {
            label: "directional/non-gating".to_string(),
            ..Default::default()
        };
    }

    let k = 10usize;
    let mut total_precision = 0.0f64;
    let mut total_recall = 0.0f64;
    let mut total_f1 = 0.0f64;

    for record in records {
        let expected: std::collections::HashSet<&str> = record
            .expected_permalinks
            .iter()
            .map(|s| s.as_str())
            .collect();

        // Count relevant results in top-k
        let relevant_in_topk = record
            .result_permalinks
            .iter()
            .take(k)
            .filter(|p| expected.contains(p.as_str()))
            .count();

        let precision = if k > 0 {
            relevant_in_topk as f64 / k as f64
        } else {
            0.0
        };
        let recall = if !expected.is_empty() {
            relevant_in_topk as f64 / expected.len() as f64
        } else {
            0.0
        };
        let f1 = if precision + recall > 0.0 {
            2.0 * precision * recall / (precision + recall)
        } else {
            0.0
        };

        total_precision += precision;
        total_recall += recall;
        total_f1 += f1;
    }

    let n = records.len() as f64;
    DirectionalMetrics {
        label: "directional/non-gating".to_string(),
        avg_precision_at_10: total_precision / n,
        avg_recall_at_10_directional: total_recall / n,
        avg_f1_at_10: total_f1 / n,
        query_count: records.len(),
    }
}

/// Compute suite-level metrics for a set of query records.
pub fn compute_suite_metrics(records: &[QueryRankRecord]) -> SuiteMetrics {
    SuiteMetrics {
        recall_at_1: recall_at_k(records, 1),
        recall_at_5: recall_at_k(records, 5),
        recall_at_10: recall_at_k(records, 10),
        mrr: mrr(records),
        zero_result_rate: zero_result_rate(records),
        query_count: records.len(),
    }
}

/// Compute aggregate metrics from per-suite metrics (weighted by query count).
///
/// **All suites contribute to the aggregate**, including `bad_cases`. The
/// aggregate reflects the full labeled Phase 1 query set (memory-ref queries
/// *and* append-only bad cases) so that `aggregate_metrics.query_count`
/// matches the total number of labeled queries. The compare policy still
/// gates each suite independently (including the bad_cases suite) and can
/// additionally gate aggregate-level thresholds.
pub fn compute_aggregate_metrics(suites: &[(&str, &SuiteMetrics)]) -> AggregateMetrics {
    let total_queries: usize = suites.iter().map(|(_, m)| m.query_count).sum();
    if total_queries == 0 {
        return AggregateMetrics::default();
    }

    let weighted = |f: fn(&SuiteMetrics) -> f64| -> f64 {
        suites
            .iter()
            .map(|(_, m)| f(m) * m.query_count as f64)
            .sum::<f64>()
            / total_queries as f64
    };

    AggregateMetrics {
        recall_at_1: weighted(|m| m.recall_at_1),
        recall_at_5: weighted(|m| m.recall_at_5),
        recall_at_10: weighted(|m| m.recall_at_10),
        mrr: weighted(|m| m.mrr),
        zero_result_rate: weighted(|m| m.zero_result_rate),
        query_count: total_queries,
    }
}

/// Per-bucket hit vectors for recall@1, recall@5, recall@10.
type BucketHits = HashMap<AgeBucket, (Vec<bool>, Vec<bool>, Vec<bool>)>;

/// Compute age-bucket recall curves.
///
/// Groups each query's expected relevant notes by age bucket and computes
/// recall@k for each bucket. This surfaces over-decay regressions.
pub fn compute_age_bucket_recall(
    records: &[QueryRankRecord],
    note_ages: &HashMap<String, u32>,
) -> HashMap<AgeBucket, RecallAtK> {
    // For each age bucket, collect per-note recall@k observations
    let mut bucket_hits: BucketHits = HashMap::new();

    for record in records {
        for (i, permalink) in record.expected_permalinks.iter().enumerate() {
            let age_days = note_ages.get(permalink).copied().unwrap_or(0);
            let bucket = AgeBucket::from_days(age_days);
            let rank = record.relevant_ranks.get(i).and_then(|r| *r);

            let entry = bucket_hits
                .entry(bucket)
                .or_insert_with(|| (Vec::new(), Vec::new(), Vec::new()));
            entry.0.push(matches!(rank, Some(r) if r <= 1));
            entry.1.push(matches!(rank, Some(r) if r <= 5));
            entry.2.push(matches!(rank, Some(r) if r <= 10));
        }
    }

    bucket_hits
        .into_iter()
        .map(|(bucket, (at1, at5, at10))| {
            let avg = |v: &[bool]| {
                if v.is_empty() {
                    0.0
                } else {
                    v.iter().filter(|b| **b).count() as f64 / v.len() as f64
                }
            };
            (
                bucket,
                RecallAtK {
                    recall_at_1: avg(&at1),
                    recall_at_5: avg(&at5),
                    recall_at_10: avg(&at10),
                },
            )
        })
        .collect()
}

// ── Compare policy ────────────────────────────────────────────────────────

/// Threshold policy version. Bump when thresholds change.
pub const THRESHOLD_POLICY_VERSION: &str = "phase1-v1";

/// Absolute threshold for suite-level recall\@k drops.
const RECALL_SUITE_DROP_THRESHOLD: f64 = 0.02;
/// Absolute threshold for suite-level MRR drops.
const MRR_SUITE_DROP_THRESHOLD: f64 = 0.02;
/// Absolute threshold for aggregate MRR drops.
const MRR_AGGREGATE_DROP_THRESHOLD: f64 = 0.01;
/// Absolute threshold for aggregate zero-result rate increases.
const ZERO_RESULT_AGGREGATE_INCREASE_THRESHOLD: f64 = 0.01;

/// Information about a specific regression detected by the compare policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegressionDetail {
    pub metric: String,
    pub suite: String,
    pub baseline_value: f64,
    pub current_value: f64,
    pub delta: f64,
    pub threshold: f64,
}

/// Information about a per-query regression.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryRegressionDetail {
    pub query_id: String,
    pub query_text: String,
    pub relevant_permalink: String,
    pub old_rank: Option<usize>,
    pub new_rank: Option<usize>,
    pub metric_delta: f64,
}

/// Result of comparing current metrics against a baseline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompareResult {
    /// Whether the comparison passed all gating thresholds.
    pub passed: bool,
    /// Gating failures that caused the comparison to fail.
    pub failures: Vec<RegressionDetail>,
    /// Per-query regression details for bad-case hit-to-miss regressions.
    pub query_regressions: Vec<QueryRegressionDetail>,
}

/// Evaluate the Phase 1 compare policy.
///
/// Thresholds (from proposal cxe1 / roadmap):
/// - Suite recall\@k drops > 0.02 absolute → fail
/// - Bad-case hit-to-miss regressions → fail
/// - Suite MRR drops > 0.02 absolute → fail
/// - Aggregate MRR drops > 0.01 absolute → fail
/// - Any bad-case zero-result increase → fail
/// - Aggregate zero-result increase > 0.01 → fail
pub fn evaluate_compare_policy(
    current_suites: &HashMap<String, SuiteMetrics>,
    current_aggregate: &AggregateMetrics,
    current_bad_case_records: &[QueryRankRecord],
    baseline_suites: &HashMap<String, SuiteMetrics>,
    baseline_aggregate: &AggregateMetrics,
    baseline_bad_case_zero_result_rate: f64,
    baseline_per_query_ranks: &HashMap<String, Vec<QueryRankBaseline>>,
) -> CompareResult {
    let mut failures = Vec::new();
    let mut query_regressions = Vec::new();

    // 1. Suite recall@k drops > 0.02 → fail
    for k_label in &["recall_at_1", "recall_at_5", "recall_at_10"] {
        for (suite_name, current) in current_suites {
            if let Some(baseline) = baseline_suites.get(suite_name) {
                let current_val = match *k_label {
                    "recall_at_1" => current.recall_at_1,
                    "recall_at_5" => current.recall_at_5,
                    "recall_at_10" => current.recall_at_10,
                    _ => unreachable!(),
                };
                let baseline_val = match *k_label {
                    "recall_at_1" => baseline.recall_at_1,
                    "recall_at_5" => baseline.recall_at_5,
                    "recall_at_10" => baseline.recall_at_10,
                    _ => unreachable!(),
                };
                let delta = current_val - baseline_val;
                if delta < -RECALL_SUITE_DROP_THRESHOLD {
                    failures.push(RegressionDetail {
                        metric: k_label.to_string(),
                        suite: suite_name.clone(),
                        baseline_value: baseline_val,
                        current_value: current_val,
                        delta,
                        threshold: RECALL_SUITE_DROP_THRESHOLD,
                    });
                }
            }
        }
    }

    // 2. Bad-case hit-to-miss regressions → fail
    //    A bad case that previously had at least one hit now has zero hits.
    //    Populate old_rank from baseline per-query rank data.
    //    Only flag regressions where the baseline actually had a hit —
    //    skip bad cases that were zero-result in the baseline too.
    for record in current_bad_case_records {
        if record.is_bad_case {
            let has_hit = record.relevant_ranks.iter().any(|r| r.is_some());
            if !has_hit && !record.expected_permalinks.is_empty() {
                // Look up baseline per-query rank for this query.
                let baseline_entry = baseline_per_query_ranks
                    .values()
                    .flatten()
                    .find(|b| b.query_id == record.query_id);

                // Only flag if the baseline had at least one hit for this query.
                let baseline_had_hit = baseline_entry
                    .map(|b| b.relevant_ranks.iter().any(|r| r.is_some()))
                    .unwrap_or(false);

                if !baseline_had_hit {
                    continue;
                }

                // This is an actual hit-to-miss regression.
                for (idx, permalink) in record.expected_permalinks.iter().enumerate() {
                    let old_rank =
                        baseline_entry.and_then(|b| b.relevant_ranks.get(idx).copied().flatten());
                    let new_rank = record.relevant_ranks.get(idx).and_then(|r| *r);
                    let metric_delta = match (old_rank, new_rank) {
                        (Some(old), Some(new)) => (1.0 / new as f64) - (1.0 / old as f64),
                        (Some(_old), None) => -1.0, // hit-to-miss: worst case
                        _ => 0.0,
                    };
                    query_regressions.push(QueryRegressionDetail {
                        query_id: record.query_id.clone(),
                        query_text: record.query_text.clone(),
                        relevant_permalink: permalink.clone(),
                        old_rank,
                        new_rank,
                        metric_delta,
                    });
                }
                failures.push(RegressionDetail {
                    metric: "bad_case_hit_to_miss".to_string(),
                    suite: "bad_cases".to_string(),
                    baseline_value: 1.0, // was a hit
                    current_value: 0.0,  // now a miss
                    delta: -1.0,
                    threshold: 0.0,
                });
            }
        }
    }

    // 3. Suite MRR drops > 0.02 → fail
    for (suite_name, current) in current_suites {
        if let Some(baseline) = baseline_suites.get(suite_name) {
            let delta = current.mrr - baseline.mrr;
            if delta < -MRR_SUITE_DROP_THRESHOLD {
                failures.push(RegressionDetail {
                    metric: "mrr".to_string(),
                    suite: suite_name.clone(),
                    baseline_value: baseline.mrr,
                    current_value: current.mrr,
                    delta,
                    threshold: MRR_SUITE_DROP_THRESHOLD,
                });
            }
        }
    }

    // 4. Aggregate MRR drops > 0.01 → fail
    let agg_mrr_delta = current_aggregate.mrr - baseline_aggregate.mrr;
    if agg_mrr_delta < -MRR_AGGREGATE_DROP_THRESHOLD {
        failures.push(RegressionDetail {
            metric: "mrr".to_string(),
            suite: "_aggregate".to_string(),
            baseline_value: baseline_aggregate.mrr,
            current_value: current_aggregate.mrr,
            delta: agg_mrr_delta,
            threshold: MRR_AGGREGATE_DROP_THRESHOLD,
        });
    }

    // 5. Any bad-case zero-result increase → fail
    let current_bad_zero = zero_result_rate(current_bad_case_records);
    if current_bad_zero > baseline_bad_case_zero_result_rate {
        failures.push(RegressionDetail {
            metric: "zero_result_rate".to_string(),
            suite: "bad_cases".to_string(),
            baseline_value: baseline_bad_case_zero_result_rate,
            current_value: current_bad_zero,
            delta: current_bad_zero - baseline_bad_case_zero_result_rate,
            threshold: 0.0,
        });
    }

    // 6. Aggregate zero-result increase > 0.01 → fail
    let agg_zr_delta = current_aggregate.zero_result_rate - baseline_aggregate.zero_result_rate;
    if agg_zr_delta > ZERO_RESULT_AGGREGATE_INCREASE_THRESHOLD {
        failures.push(RegressionDetail {
            metric: "zero_result_rate".to_string(),
            suite: "_aggregate".to_string(),
            baseline_value: baseline_aggregate.zero_result_rate,
            current_value: current_aggregate.zero_result_rate,
            delta: agg_zr_delta,
            threshold: ZERO_RESULT_AGGREGATE_INCREASE_THRESHOLD,
        });
    }

    CompareResult {
        passed: failures.is_empty(),
        failures,
        query_regressions,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
