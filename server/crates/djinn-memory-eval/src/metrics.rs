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
mod tests {
    use super::*;
    use crate::fixtures::BadCaseType;

    fn make_record(
        query_id: &str,
        expected: Vec<&str>,
        result_ranks: Vec<Option<usize>>,
        is_bad_case: bool,
    ) -> QueryRankRecord {
        let expected_permalinks: Vec<String> = expected.into_iter().map(String::from).collect();
        // Build result_permalinks from ranks
        let max_rank = result_ranks.iter().filter_map(|r| *r).max().unwrap_or(0);
        let mut result_permalinks: Vec<String> =
            (1..=max_rank).map(|i| format!("result-{}", i)).collect();
        // Place expected permalinks at their ranks
        for (i, permalink) in expected_permalinks.iter().enumerate() {
            if let Some(Some(rank)) = result_ranks.get(i)
                && *rank <= result_permalinks.len()
            {
                result_permalinks[*rank - 1] = permalink.clone();
            }
        }

        let relevant_ranks = result_ranks;
        QueryRankRecord {
            query_id: query_id.to_string(),
            query_text: format!("query for {}", query_id),
            task_id: None,
            result_permalinks,
            relevant_ranks,
            expected_permalinks,
            is_bad_case,
            bad_case_type: if is_bad_case {
                Some(BadCaseType::RankRegression)
            } else {
                None
            },
        }
    }

    // ── recall@k ──────────────────────────────────────────────────────────

    #[test]
    fn recall_at_k_perfect_when_all_found() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(1)], false),
            make_record("q2", vec!["b"], vec![Some(3)], false),
        ];
        assert!((recall_at_k(&records, 1) - 0.5).abs() < 1e-10);
        assert!((recall_at_k(&records, 5) - 1.0).abs() < 1e-10);
        assert!((recall_at_k(&records, 10) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn recall_at_k_zero_when_none_found() {
        let records = vec![
            make_record("q1", vec!["a"], vec![None], false),
            make_record("q2", vec!["b"], vec![None], false),
        ];
        assert!((recall_at_k(&records, 1)).abs() < 1e-10);
        assert!((recall_at_k(&records, 5)).abs() < 1e-10);
        assert!((recall_at_k(&records, 10)).abs() < 1e-10);
    }

    #[test]
    fn recall_at_k_empty_records() {
        let records: Vec<QueryRankRecord> = vec![];
        assert!((recall_at_k(&records, 1)).abs() < 1e-10);
    }

    #[test]
    fn recall_at_k_partial() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(1)], false), // found at 1
            make_record("q2", vec!["b"], vec![Some(7)], false), // found at 7
            make_record("q3", vec!["c"], vec![None], false),    // not found
        ];
        // recall@1: only q1
        assert!((recall_at_k(&records, 1) - 1.0 / 3.0).abs() < 1e-10);
        // recall@5: only q1
        assert!((recall_at_k(&records, 5) - 1.0 / 3.0).abs() < 1e-10);
        // recall@10: q1 and q2
        assert!((recall_at_k(&records, 10) - 2.0 / 3.0).abs() < 1e-10);
    }

    // ── MRR ───────────────────────────────────────────────────────────────

    #[test]
    fn mrr_perfect() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(1)], false),
            make_record("q2", vec!["b"], vec![Some(1)], false),
        ];
        assert!((mrr(&records) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn mrr_rank_2() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(2)], false),
            make_record("q2", vec!["b"], vec![Some(2)], false),
        ];
        assert!((mrr(&records) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn mrr_mixed() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(1)], false), // RR = 1.0
            make_record("q2", vec!["b"], vec![Some(4)], false), // RR = 0.25
            make_record("q3", vec!["c"], vec![None], false),    // RR = 0.0
        ];
        // MRR = (1.0 + 0.25 + 0.0) / 3 = 0.4167
        let expected = (1.0 + 0.25) / 3.0;
        assert!((mrr(&records) - expected).abs() < 1e-10);
    }

    #[test]
    fn mrr_empty() {
        let records: Vec<QueryRankRecord> = vec![];
        assert!((mrr(&records)).abs() < 1e-10);
    }

    #[test]
    fn mrr_uses_best_rank() {
        // Query has 2 relevant notes; best rank is 2
        let record = QueryRankRecord {
            query_id: "q1".to_string(),
            query_text: "test".to_string(),
            task_id: None,
            result_permalinks: vec!["x".to_string(), "a".to_string(), "b".to_string()],
            relevant_ranks: vec![Some(2), Some(3)],
            expected_permalinks: vec!["a".to_string(), "b".to_string()],
            is_bad_case: false,
            bad_case_type: None,
        };
        // RR = 1/2 = 0.5
        assert!((mrr(&[record]) - 0.5).abs() < 1e-10);
    }

    // ── zero-result rate ──────────────────────────────────────────────────

    #[test]
    fn zero_result_rate_all_found() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(1)], false),
            make_record("q2", vec!["b"], vec![Some(5)], false),
        ];
        assert!((zero_result_rate(&records)).abs() < 1e-10);
    }

    #[test]
    fn zero_result_rate_all_missed() {
        let records = vec![
            make_record("q1", vec!["a"], vec![None], false),
            make_record("q2", vec!["b"], vec![None], false),
        ];
        assert!((zero_result_rate(&records) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn zero_result_rate_partial() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(1)], false),
            make_record("q2", vec!["b"], vec![None], false),
        ];
        assert!((zero_result_rate(&records) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn zero_result_rate_empty() {
        let records: Vec<QueryRankRecord> = vec![];
        assert!((zero_result_rate(&records)).abs() < 1e-10);
    }

    // ── directional metrics ───────────────────────────────────────────────

    #[test]
    fn directional_metrics_perfect() {
        let records = vec![make_record(
            "q1",
            vec!["a", "b"],
            vec![Some(1), Some(2)],
            false,
        )];
        let dm = directional_metrics(&records);
        assert_eq!(dm.label, "directional/non-gating");
        // precision@10 = 2/10 = 0.2
        assert!((dm.avg_precision_at_10 - 0.2).abs() < 1e-10);
        // recall@10 = 2/2 = 1.0
        assert!((dm.avg_recall_at_10_directional - 1.0).abs() < 1e-10);
    }

    #[test]
    fn directional_metrics_labeled_as_non_gating() {
        let records = vec![make_record("q1", vec!["a"], vec![Some(1)], false)];
        let dm = directional_metrics(&records);
        assert_eq!(dm.label, "directional/non-gating");
    }

    #[test]
    fn directional_metrics_empty() {
        let records: Vec<QueryRankRecord> = vec![];
        let dm = directional_metrics(&records);
        assert_eq!(dm.label, "directional/non-gating");
        assert_eq!(dm.query_count, 0);
    }

    // ── suite metrics ─────────────────────────────────────────────────────

    #[test]
    fn compute_suite_metrics_correctness() {
        let records = vec![
            make_record("q1", vec!["a"], vec![Some(1)], false),
            make_record("q2", vec!["b"], vec![Some(6)], false),
            make_record("q3", vec!["c"], vec![None], false),
        ];
        let m = compute_suite_metrics(&records);
        assert_eq!(m.query_count, 3);
        // recall@1: q1 only = 1/3
        assert!((m.recall_at_1 - 1.0 / 3.0).abs() < 1e-10);
        // recall@5: q1 only = 1/3
        assert!((m.recall_at_5 - 1.0 / 3.0).abs() < 1e-10);
        // recall@10: q1, q2 = 2/3
        assert!((m.recall_at_10 - 2.0 / 3.0).abs() < 1e-10);
        // MRR = (1/1 + 1/6 + 0) / 3 = (1 + 0.1667) / 3
        let expected_mrr = (1.0 + 1.0 / 6.0) / 3.0;
        assert!((m.mrr - expected_mrr).abs() < 1e-10);
        // zero-result: q3 only = 1/3
        assert!((m.zero_result_rate - 1.0 / 3.0).abs() < 1e-10);
    }

    // ── age bucket ────────────────────────────────────────────────────────

    #[test]
    fn age_bucket_classification() {
        assert_eq!(AgeBucket::from_days(0), AgeBucket::Under7d);
        assert_eq!(AgeBucket::from_days(6), AgeBucket::Under7d);
        assert_eq!(AgeBucket::from_days(7), AgeBucket::Days7to30);
        assert_eq!(AgeBucket::from_days(29), AgeBucket::Days7to30);
        assert_eq!(AgeBucket::from_days(30), AgeBucket::Days30to90);
        assert_eq!(AgeBucket::from_days(89), AgeBucket::Days30to90);
        assert_eq!(AgeBucket::from_days(90), AgeBucket::OverDecayThreshold);
        assert_eq!(AgeBucket::from_days(365), AgeBucket::OverDecayThreshold);
    }

    #[test]
    fn age_bucket_recall_computation() {
        let records = vec![
            make_record("q1", vec!["fresh"], vec![Some(1)], false),
            make_record("q2", vec!["old"], vec![Some(8)], false),
        ];
        let mut note_ages = HashMap::new();
        note_ages.insert("fresh".to_string(), 3); // <7d
        note_ages.insert("old".to_string(), 100); // >90d

        let buckets = compute_age_bucket_recall(&records, &note_ages);

        let fresh = buckets.get(&AgeBucket::Under7d).unwrap();
        assert!((fresh.recall_at_1 - 1.0).abs() < 1e-10);
        assert!((fresh.recall_at_10 - 1.0).abs() < 1e-10);

        let old = buckets.get(&AgeBucket::OverDecayThreshold).unwrap();
        assert!((old.recall_at_1).abs() < 1e-10); // rank 8, not <=1
        assert!((old.recall_at_5).abs() < 1e-10); // rank 8, not <=5
        assert!((old.recall_at_10 - 1.0).abs() < 1e-10); // rank 8, <=10
    }

    #[test]
    fn age_bucket_recall_over_decay_bucket_present() {
        // Verify the over-decay-threshold bucket is always produced
        // when there are notes older than 90 days.
        let records = vec![make_record("q1", vec!["ancient"], vec![Some(1)], false)];
        let mut note_ages = HashMap::new();
        note_ages.insert("ancient".to_string(), 200);

        let buckets = compute_age_bucket_recall(&records, &note_ages);
        assert!(
            buckets.contains_key(&AgeBucket::OverDecayThreshold),
            "over-decay-threshold bucket must be present for notes >90d"
        );
    }

    // ── aggregate metrics ─────────────────────────────────────────────────

    #[test]
    fn aggregate_metrics_weighted_average() {
        let suite_a = SuiteMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.85,
            zero_result_rate: 0.1,
            query_count: 10,
        };
        let suite_b = SuiteMetrics {
            recall_at_1: 0.6,
            recall_at_5: 0.7,
            recall_at_10: 0.8,
            mrr: 0.65,
            zero_result_rate: 0.2,
            query_count: 5,
        };
        let suites = vec![("a", &suite_a), ("b", &suite_b)];
        let agg = compute_aggregate_metrics(&suites);

        // weighted: (0.8*10 + 0.6*5) / 15 = (8+3)/15 = 11/15
        let expected_r1 = (0.8 * 10.0 + 0.6 * 5.0) / 15.0;
        assert!((agg.recall_at_1 - expected_r1).abs() < 1e-10);
        assert_eq!(agg.query_count, 15);
    }

    #[test]
    fn aggregate_includes_bad_cases() {
        let suite_good = SuiteMetrics {
            recall_at_1: 1.0,
            recall_at_5: 1.0,
            recall_at_10: 1.0,
            mrr: 1.0,
            zero_result_rate: 0.0,
            query_count: 5,
        };
        let suite_bad = SuiteMetrics {
            recall_at_1: 0.0,
            recall_at_5: 0.0,
            recall_at_10: 0.0,
            mrr: 0.0,
            zero_result_rate: 1.0,
            query_count: 100,
        };
        let suites = vec![("good", &suite_good), ("bad_cases", &suite_bad)];
        let agg = compute_aggregate_metrics(&suites);
        // Aggregate now includes ALL suites (good + bad_cases).
        // Weighted recall@1: (1.0*5 + 0.0*100) / 105 = 5/105
        let expected_r1 = 5.0 / 105.0;
        assert!(
            (agg.recall_at_1 - expected_r1).abs() < 1e-10,
            "expected aggregate recall@1 = {expected_r1}, got {}",
            agg.recall_at_1
        );
        assert_eq!(agg.query_count, 105);
    }

    /// Aggregate count must equal the sum of ALL suite query counts,
    /// including bad_cases. This is the critical regression guard.
    #[test]
    fn aggregate_count_equals_sum_of_all_suites() {
        let suite_queries = SuiteMetrics {
            recall_at_1: 0.0,
            recall_at_5: 0.0,
            recall_at_10: 0.0,
            mrr: 0.0,
            zero_result_rate: 1.0,
            query_count: 17,
        };
        let suite_bad = SuiteMetrics {
            recall_at_1: 0.2,
            recall_at_5: 0.2,
            recall_at_10: 0.2,
            mrr: 0.2,
            zero_result_rate: 0.8,
            query_count: 10,
        };
        let suites = vec![("all_queries", &suite_queries), ("bad_cases", &suite_bad)];
        let agg = compute_aggregate_metrics(&suites);
        assert_eq!(
            agg.query_count, 27,
            "aggregate query_count must be 17 + 10 = 27, got {}",
            agg.query_count
        );
    }

    // ── compare policy ────────────────────────────────────────────────────

    fn make_suite(r1: f64, r5: f64, r10: f64, mrr_val: f64, zr: f64, count: usize) -> SuiteMetrics {
        SuiteMetrics {
            recall_at_1: r1,
            recall_at_5: r5,
            recall_at_10: r10,
            mrr: mrr_val,
            zero_result_rate: zr,
            query_count: count,
        }
    }

    #[test]
    fn compare_passes_when_no_regressions() {
        let mut current = HashMap::new();
        current.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.85, 0.0, 10),
        );
        let mut baseline = HashMap::new();
        baseline.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.85, 0.0, 10),
        );

        let current_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.85,
            zero_result_rate: 0.0,
            query_count: 10,
        };
        let baseline_agg = current_agg.clone();

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &[],
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        assert!(result.passed, "should pass with no regressions");
        assert!(result.failures.is_empty());
    }

    #[test]
    fn compare_fails_on_recall_drop() {
        let mut current = HashMap::new();
        current.insert(
            "all_queries".to_string(),
            make_suite(0.75, 0.85, 0.95, 0.80, 0.0, 10),
        );
        let mut baseline = HashMap::new();
        baseline.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.85, 0.0, 10),
        );

        let current_agg = AggregateMetrics {
            recall_at_1: 0.75,
            recall_at_5: 0.85,
            recall_at_10: 0.95,
            mrr: 0.80,
            zero_result_rate: 0.0,
            query_count: 10,
        };
        let baseline_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.85,
            zero_result_rate: 0.0,
            query_count: 10,
        };

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &[],
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        // recall@1 dropped 0.05 (> 0.02 threshold)
        assert!(!result.passed, "should fail on recall@1 drop > 0.02");
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.metric == "recall_at_1" && f.suite == "all_queries")
        );
    }

    #[test]
    fn compare_fails_on_suite_mrr_drop() {
        let mut current = HashMap::new();
        current.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.78, 0.0, 10),
        );
        let mut baseline = HashMap::new();
        baseline.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.85, 0.0, 10),
        );

        let current_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.78,
            zero_result_rate: 0.0,
            query_count: 10,
        };
        let baseline_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.85,
            zero_result_rate: 0.0,
            query_count: 10,
        };

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &[],
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        // MRR dropped 0.07 (> 0.02 suite threshold)
        assert!(!result.passed, "should fail on suite MRR drop > 0.02");
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.metric == "mrr" && f.suite == "all_queries")
        );
    }

    #[test]
    fn compare_fails_on_aggregate_mrr_drop() {
        let mut current = HashMap::new();
        current.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.84, 0.0, 10),
        );
        let mut baseline = HashMap::new();
        baseline.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.85, 0.0, 10),
        );

        // Suite MRR drop is 0.01 (< 0.02 suite threshold) — not a suite failure
        let current_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.84,
            zero_result_rate: 0.0,
            query_count: 10,
        };
        let baseline_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.855,
            zero_result_rate: 0.0,
            query_count: 10,
        };

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &[],
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        // Aggregate MRR dropped 0.015 (> 0.01 aggregate threshold)
        assert!(!result.passed, "should fail on aggregate MRR drop > 0.01");
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.metric == "mrr" && f.suite == "_aggregate")
        );
    }

    #[test]
    fn compare_fails_on_bad_case_zero_result_increase() {
        let mut current = HashMap::new();
        current.insert(
            "bad_cases".to_string(),
            make_suite(0.0, 0.0, 0.0, 0.0, 0.5, 2),
        );
        let mut baseline = HashMap::new();
        baseline.insert(
            "bad_cases".to_string(),
            make_suite(0.0, 0.0, 1.0, 0.1, 0.0, 2),
        );

        let current_agg = AggregateMetrics::default();
        let baseline_agg = AggregateMetrics::default();

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &[],
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        // Bad-case zero-result went from 0.0 to 0.5 (any increase fails)
        assert!(
            !result.passed,
            "should fail on bad-case zero-result increase"
        );
    }

    #[test]
    fn compare_fails_on_aggregate_zero_result_increase() {
        let current = HashMap::new();
        let baseline = HashMap::new();

        let current_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.85,
            zero_result_rate: 0.05,
            query_count: 10,
        };
        let baseline_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.85,
            zero_result_rate: 0.03,
            query_count: 10,
        };

        // Zero-result increased by 0.02 (> 0.01 threshold)
        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &[],
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        assert!(
            !result.passed,
            "should fail on aggregate zero-result increase > 0.01"
        );
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.metric == "zero_result_rate" && f.suite == "_aggregate")
        );
    }

    #[test]
    fn compare_tolerates_small_recall_drop() {
        // Drops smaller than threshold should pass.
        // Use 0.015 drop (< 0.02) to avoid floating-point boundary issues.
        let mut current = HashMap::new();
        current.insert(
            "all_queries".to_string(),
            make_suite(0.79, 0.89, 0.99, 0.84, 0.0, 10),
        );
        let mut baseline = HashMap::new();
        baseline.insert(
            "all_queries".to_string(),
            make_suite(0.8, 0.9, 1.0, 0.85, 0.0, 10),
        );

        // Aggregate MRR drop of 0.01 is at threshold — but we use 0.005 < 0.01
        let current_agg = AggregateMetrics {
            recall_at_1: 0.79,
            recall_at_5: 0.89,
            recall_at_10: 0.99,
            mrr: 0.845,
            zero_result_rate: 0.0,
            query_count: 10,
        };
        let baseline_agg = AggregateMetrics {
            recall_at_1: 0.8,
            recall_at_5: 0.9,
            recall_at_10: 1.0,
            mrr: 0.85,
            zero_result_rate: 0.0,
            query_count: 10,
        };

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &[],
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        // Drops smaller than threshold should pass
        assert!(
            result.passed,
            "should pass when drops are smaller than threshold. failures: {:?}",
            result.failures
        );
    }

    #[test]
    fn compare_fails_on_bad_case_hit_to_miss() {
        let bad_records = vec![make_record(
            "bc-001",
            vec!["note-a"],
            vec![None], // was a hit, now a miss
            true,
        )];

        let current = HashMap::new();
        let baseline = HashMap::new();
        let current_agg = AggregateMetrics::default();
        let baseline_agg = AggregateMetrics::default();

        // Provide baseline per-query ranks showing this bad case was a hit at rank 3.
        let mut baseline_per_query = HashMap::new();
        baseline_per_query.insert(
            "bad_cases".to_string(),
            vec![QueryRankBaseline {
                query_id: "bc-001".to_string(),
                query_text: "query for bc-001".to_string(),
                result_permalinks: vec!["note-a".to_string()],
                relevant_ranks: vec![Some(3)],
                best_rank: Some(3),
            }],
        );

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &bad_records,
            &baseline,
            &baseline_agg,
            0.0,
            &baseline_per_query,
        );
        assert!(!result.passed, "should fail on bad-case hit-to-miss");
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.metric == "bad_case_hit_to_miss")
        );
        assert!(!result.query_regressions.is_empty());
        // Verify old_rank is populated from the baseline
        let reg = &result.query_regressions[0];
        assert_eq!(reg.old_rank, Some(3), "old_rank should come from baseline");
        assert_eq!(reg.new_rank, None, "new_rank should be None (miss)");
    }

    /// A bad case that was also zero in the baseline should NOT trigger a
    /// hit-to-miss regression (it was never a hit).
    #[test]
    fn compare_skips_bad_case_that_was_also_miss_in_baseline() {
        let bad_records = vec![make_record(
            "bc-002",
            vec!["note-b"],
            vec![None], // miss in current
            true,
        )];

        let current = HashMap::new();
        let baseline = HashMap::new();
        let current_agg = AggregateMetrics::default();
        let baseline_agg = AggregateMetrics::default();

        // Provide baseline per-query ranks showing this bad case was ALSO a miss.
        let mut baseline_per_query = HashMap::new();
        baseline_per_query.insert(
            "bad_cases".to_string(),
            vec![QueryRankBaseline {
                query_id: "bc-002".to_string(),
                query_text: "query for bc-002".to_string(),
                result_permalinks: vec![],
                relevant_ranks: vec![None], // baseline also had no hit
                best_rank: None,
            }],
        );

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &bad_records,
            &baseline,
            &baseline_agg,
            1.0, // baseline also had 100% zero-result bad cases
            &baseline_per_query,
        );
        // Should NOT fail: the bad case was already a miss in the baseline.
        assert!(
            result.passed,
            "should pass when bad case was also a miss in baseline"
        );
        assert!(result.query_regressions.is_empty());
    }

    #[test]
    fn compare_passes_when_bad_case_still_has_hit() {
        let bad_records = vec![make_record(
            "bc-001",
            vec!["note-a"],
            vec![Some(3)], // still a hit
            true,
        )];

        let current = HashMap::new();
        let baseline = HashMap::new();
        let current_agg = AggregateMetrics::default();
        let baseline_agg = AggregateMetrics::default();

        let result = evaluate_compare_policy(
            &current,
            &current_agg,
            &bad_records,
            &baseline,
            &baseline_agg,
            0.0,
            &HashMap::new(),
        );
        assert!(result.passed, "should pass when bad case still has a hit");
    }

    #[test]
    fn threshold_policy_version_is_set() {
        assert_eq!(THRESHOLD_POLICY_VERSION, "phase1-v1");
    }
}
