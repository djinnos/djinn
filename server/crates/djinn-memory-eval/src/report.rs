//! Report generation for Phase 1 benchmark results.
//!
//! Emits `target/memory-eval/phase1-report.json` and
//! `target/memory-eval/phase1-summary.md` with per-query regression details.
//!
//! # Report structure
//!
//! - **phase1-report.json**: Full machine-readable report including per-suite
//!   metrics, per-query records with age data, age-bucket recall, directional
//!   metrics, and compare results (when available).
//! - **phase1-summary.md**: Human-readable Markdown summary with per-query
//!   regression details: query id, query text, relevant permalink, old rank,
//!   new rank, and metric delta.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::metrics::{
    AgeBucket, AggregateMetrics, CompareResult, DirectionalMetrics, QueryResultRecord, RecallAtK,
    SuiteMetrics,
};

// ── Report types ──────────────────────────────────────────────────────────

/// Full Phase 1 benchmark report. Serializable to JSON for downstream
/// consumption by the `compare` and `refresh-baseline` commands.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Phase1Report {
    /// Per-suite metrics (gating).
    pub suite_metrics: HashMap<String, SuiteMetrics>,
    /// Aggregate metrics across non-bad-case suites.
    pub aggregate_metrics: AggregateMetrics,
    /// Age-bucket recall curves.
    pub age_bucket_recall: HashMap<AgeBucket, RecallAtK>,
    /// Directional (non-gating) precision/F1 metrics.
    pub directional: DirectionalMetrics,
    /// Per-query result records with age data.
    pub query_records: Vec<QueryResultRecord>,
    /// Signal comparison records (graph/entity and task-affinity assertions).
    #[serde(default)]
    pub signal_comparisons: Vec<crate::run::SignalRankComparison>,
    /// Compare result (populated by the `compare` command, None for bare `run`).
    #[serde(default)]
    pub compare_result: Option<CompareResult>,
    /// Threshold policy version used for compare.
    pub threshold_policy_version: String,
    /// Fixture hashes from the manifest (if available).
    #[serde(default)]
    pub fixture_hashes: Option<crate::fixtures::FixtureFileHashes>,
}

/// Phase 1 baseline file structure.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Phase1Baseline {
    /// Metadata about the baseline.
    pub metadata: BaselineMetadata,
    /// Per-suite metrics.
    pub suite_metrics: HashMap<String, SuiteMetrics>,
    /// Aggregate metrics.
    pub aggregate_metrics: AggregateMetrics,
    /// Age-bucket recall curves.
    pub age_bucket_recall: HashMap<AgeBucket, RecallAtK>,
    /// Per-query top-k ranks, keyed by suite name.
    pub per_query_ranks: HashMap<String, Vec<QueryRankBaseline>>,
    /// Signal comparison records (graph/entity and task-affinity assertions).
    /// Proves which queries/cases demonstrate rank-change coverage for each signal.
    #[serde(default)]
    pub signal_comparisons: Vec<crate::run::SignalRankComparison>,
    /// Threshold policy version.
    pub threshold_policy_version: String,
}

/// Metadata for a baseline file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineMetadata {
    /// Fixture hashes for integrity verification.
    pub fixture_hashes: Option<crate::fixtures::FixtureFileHashes>,
    /// Git commit SHA when the baseline was refreshed.
    pub refresh_commit: String,
    /// ISO-8601 timestamp when the baseline was created.
    pub created_at: String,
}

/// Per-query baseline rank information.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryRankBaseline {
    pub query_id: String,
    pub query_text: String,
    pub result_permalinks: Vec<String>,
    pub relevant_ranks: Vec<Option<usize>>,
    pub best_rank: Option<usize>,
}

// ── Report generation ─────────────────────────────────────────────────────

/// Write the Phase 1 report JSON and summary Markdown.
pub fn write_reports(report: &Phase1Report, target_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(target_dir)
        .with_context(|| format!("creating target dir {}", target_dir.display()))?;

    // Write JSON report
    let json_path = target_dir.join("phase1-report.json");
    let json = serde_json::to_string_pretty(report).context("serializing phase1-report.json")?;
    std::fs::write(&json_path, &json)
        .with_context(|| format!("writing {}", json_path.display()))?;

    // Write Markdown summary
    let md_path = target_dir.join("phase1-summary.md");
    let md = render_summary_markdown(report);
    std::fs::write(&md_path, &md).with_context(|| format!("writing {}", md_path.display()))?;

    Ok(())
}

/// Render the Phase 1 summary as Markdown.
fn render_summary_markdown(report: &Phase1Report) -> String {
    let mut out = String::new();

    out.push_str("# Phase 1 Memory Eval — Benchmark Summary\n\n");

    // ── Aggregate metrics ─────────────────────────────────────────────────
    out.push_str("## Aggregate Metrics (Gating)\n\n");
    out.push_str("| Metric | Value |\n|--------|-------|\n");
    let agg = &report.aggregate_metrics;
    out.push_str(&format!("| recall\\@1 | {:.4} |\n", agg.recall_at_1));
    out.push_str(&format!("| recall\\@5 | {:.4} |\n", agg.recall_at_5));
    out.push_str(&format!("| recall\\@10 | {:.4} |\n", agg.recall_at_10));
    out.push_str(&format!("| MRR | {:.4} |\n", agg.mrr));
    out.push_str(&format!(
        "| Zero-result rate | {:.4} |\n",
        agg.zero_result_rate
    ));
    out.push_str(&format!("| Query count | {} |\n\n", agg.query_count));

    // ── Per-suite metrics ─────────────────────────────────────────────────
    out.push_str("## Per-Suite Metrics (Gating)\n\n");
    for (suite_name, metrics) in &report.suite_metrics {
        out.push_str(&format!("### {}\n\n", suite_name));
        out.push_str("| Metric | Value |\n|--------|-------|\n");
        out.push_str(&format!("| recall\\@1 | {:.4} |\n", metrics.recall_at_1));
        out.push_str(&format!("| recall\\@5 | {:.4} |\n", metrics.recall_at_5));
        out.push_str(&format!("| recall\\@10 | {:.4} |\n", metrics.recall_at_10));
        out.push_str(&format!("| MRR | {:.4} |\n", metrics.mrr));
        out.push_str(&format!(
            "| Zero-result rate | {:.4} |\n",
            metrics.zero_result_rate
        ));
        out.push_str(&format!("| Query count | {} |\n\n", metrics.query_count));
    }

    // ── Age-bucket recall ─────────────────────────────────────────────────
    out.push_str("## Age-Bucket Recall Curves\n\n");
    out.push_str("| Age Bucket | recall\\@1 | recall\\@5 | recall\\@10 |\n");
    out.push_str("|------------|-----------|-----------|------------|\n");
    for bucket in AgeBucket::all() {
        if let Some(recall) = report.age_bucket_recall.get(bucket) {
            out.push_str(&format!(
                "| {} | {:.4} | {:.4} | {:.4} |\n",
                bucket, recall.recall_at_1, recall.recall_at_5, recall.recall_at_10
            ));
        }
    }
    out.push('\n');

    // ── Directional metrics ───────────────────────────────────────────────
    out.push_str("## Directional Metrics (Non-Gating)\n\n");
    out.push_str(&format!(
        "_Label: {} — mined `tasks.memory_refs` labels are sparse; these \
         metrics are informational only._\n\n",
        report.directional.label
    ));
    out.push_str("| Metric | Value |\n|--------|-------|\n");
    out.push_str(&format!(
        "| Precision\\@10 | {:.4} |\n",
        report.directional.avg_precision_at_10
    ));
    out.push_str(&format!(
        "| Recall\\@10 (directional) | {:.4} |\n",
        report.directional.avg_recall_at_10_directional
    ));
    out.push_str(&format!(
        "| F1\\@10 | {:.4} |\n\n",
        report.directional.avg_f1_at_10
    ));

    // ── Compare result ────────────────────────────────────────────────────
    if let Some(ref compare) = report.compare_result {
        out.push_str("## Compare Result\n\n");
        if compare.passed {
            out.push_str("**Status: ✅ PASSED** — no gating regressions detected.\n\n");
        } else {
            out.push_str(&format!(
                "**Status: ❌ FAILED** — {} gating failure(s) detected.\n\n",
                compare.failures.len()
            ));

            out.push_str("### Gating Failures\n\n");
            out.push_str("| Metric | Suite | Baseline | Current | Delta | Threshold |\n");
            out.push_str("|--------|-------|----------|---------|-------|-----------|\n");
            for failure in &compare.failures {
                out.push_str(&format!(
                    "| {} | {} | {:.4} | {:.4} | {:+.4} | {:.4} |\n",
                    failure.metric,
                    failure.suite,
                    failure.baseline_value,
                    failure.current_value,
                    failure.delta,
                    failure.threshold,
                ));
            }
            out.push('\n');
        }

        // ── Per-query regression details ─────────────────────────────────────
        if !compare.query_regressions.is_empty() {
            out.push_str("### Per-Query Regression Details\n\n");
            out.push_str(
                "| Query ID | Query Text | Relevant Permalink | Old Rank | New Rank | Metric Delta |\n",
            );
            out.push_str(
                "|----------|------------|-------------------|----------|----------|-------------|\n",
            );
            for reg in &compare.query_regressions {
                let old_rank_str = reg.old_rank.map_or("—".to_string(), |r| r.to_string());
                let new_rank_str = reg.new_rank.map_or("—".to_string(), |r| r.to_string());
                let truncated_text = if reg.query_text.len() > 60 {
                    format!("{}…", &reg.query_text[..57])
                } else {
                    reg.query_text.clone()
                };
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {:+.4} |\n",
                    reg.query_id,
                    truncated_text,
                    reg.relevant_permalink,
                    old_rank_str,
                    new_rank_str,
                    reg.metric_delta,
                ));
            }
            out.push('\n');
        }
    }

    // ── Signal comparisons ────────────────────────────────────────────────
    if !report.signal_comparisons.is_empty() {
        out.push_str("## Signal Comparison Details\n\n");
        out.push_str(
            "These comparisons prove that graph/entity and task-affinity inputs \
             each change at least one relevant note rank, preventing silent \
             collapse to lexical/vector/temporal-only behavior.\n\n",
        );
        out.push_str("| Query ID | Signal | Rank With | Rank Without | Changed |\n");
        out.push_str("|----------|--------|-----------|--------------|---------|\n");
        for sc in &report.signal_comparisons {
            let with_str = sc
                .rank_with_signal
                .map_or("—".to_string(), |r| r.to_string());
            let without_str = sc
                .rank_without_signal
                .map_or("—".to_string(), |r| r.to_string());
            let changed_str = if sc.rank_changed { "✅" } else { "—" };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                sc.query_id, sc.signal, with_str, without_str, changed_str,
            ));
        }
        out.push('\n');
    }

    // ── Threshold policy ──────────────────────────────────────────────────
    out.push_str(&format!(
        "_Threshold policy version: {}_\n",
        report.threshold_policy_version
    ));

    out
}

// ── Baseline I/O ──────────────────────────────────────────────────────────

/// Load a Phase 1 baseline from disk.
pub fn load_baseline(crate_root: &Path) -> Result<Phase1Baseline> {
    let path = crate_root.join("baselines").join("phase1.json");
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("reading baseline {}", path.display()))?;
    let baseline: Phase1Baseline = serde_json::from_str(&data).context("parsing baseline JSON")?;
    Ok(baseline)
}

/// Write a Phase 1 baseline to disk.
pub fn write_baseline(crate_root: &Path, baseline: &Phase1Baseline) -> Result<()> {
    let dir = crate_root.join("baselines");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating baselines dir {}", dir.display()))?;
    let path = dir.join("phase1.json");
    let json = serde_json::to_string_pretty(baseline).context("serializing baseline")?;
    std::fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Build a baseline from a Phase1Report and metadata.
pub fn build_baseline(
    report: &Phase1Report,
    fixture_hashes: Option<crate::fixtures::FixtureFileHashes>,
    refresh_commit: String,
    epoch_secs: u64,
) -> Phase1Baseline {
    let mut per_query_ranks: HashMap<String, Vec<QueryRankBaseline>> = HashMap::new();

    // Group query records by suite
    let mut all_queries_ranks = Vec::new();
    let mut bad_case_ranks = Vec::new();

    for record in &report.query_records {
        let baseline_rank = QueryRankBaseline {
            query_id: record.query_id.clone(),
            query_text: record.query_text.clone(),
            result_permalinks: record.result_permalinks.clone(),
            relevant_ranks: record.relevant_ranks.clone(),
            best_rank: record.best_rank,
        };
        if record.is_bad_case {
            bad_case_ranks.push(baseline_rank);
        } else {
            all_queries_ranks.push(baseline_rank);
        }
    }

    per_query_ranks.insert("all_queries".to_string(), all_queries_ranks);
    if !bad_case_ranks.is_empty() {
        per_query_ranks.insert("bad_cases".to_string(), bad_case_ranks);
    }

    Phase1Baseline {
        metadata: BaselineMetadata {
            fixture_hashes,
            refresh_commit,
            created_at: format_timestamp(epoch_secs),
        },
        suite_metrics: report.suite_metrics.clone(),
        aggregate_metrics: report.aggregate_metrics.clone(),
        age_bucket_recall: report.age_bucket_recall.clone(),
        per_query_ranks,
        signal_comparisons: report.signal_comparisons.clone(),
        threshold_policy_version: report.threshold_policy_version.clone(),
    }
}

/// Format an epoch-seconds timestamp as ISO-8601 string (UTC).
fn format_timestamp(epoch_secs: u64) -> String {
    let total_days = (epoch_secs / 86400) as i64;
    let time_of_day = epoch_secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Civil calendar from days (Howard Hinnant algorithm, using signed arithmetic)
    let z = total_days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hours, minutes, seconds
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::DirectionalMetrics;
    use crate::metrics::THRESHOLD_POLICY_VERSION;

    fn make_test_report() -> Phase1Report {
        let mut suite_metrics = HashMap::new();
        suite_metrics.insert(
            "all_queries".to_string(),
            SuiteMetrics {
                recall_at_1: 0.8,
                recall_at_5: 0.9,
                recall_at_10: 1.0,
                mrr: 0.85,
                zero_result_rate: 0.0,
                query_count: 10,
            },
        );

        Phase1Report {
            suite_metrics,
            aggregate_metrics: AggregateMetrics {
                recall_at_1: 0.8,
                recall_at_5: 0.9,
                recall_at_10: 1.0,
                mrr: 0.85,
                zero_result_rate: 0.0,
                query_count: 10,
            },
            age_bucket_recall: HashMap::new(),
            directional: DirectionalMetrics {
                label: "directional/non-gating".to_string(),
                avg_precision_at_10: 0.3,
                avg_recall_at_10_directional: 0.8,
                avg_f1_at_10: 0.43,
                query_count: 10,
            },
            query_records: vec![],
            signal_comparisons: vec![],
            compare_result: None,
            threshold_policy_version: THRESHOLD_POLICY_VERSION.to_string(),
            fixture_hashes: None,
        }
    }

    #[test]
    fn report_round_trips_json() {
        let report = make_test_report();
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: Phase1Report = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.threshold_policy_version, THRESHOLD_POLICY_VERSION);
        assert_eq!(parsed.aggregate_metrics.query_count, 10);
    }

    #[test]
    fn summary_markdown_includes_aggregate_metrics() {
        let report = make_test_report();
        let md = render_summary_markdown(&report);
        assert!(md.contains("Aggregate Metrics"));
        assert!(md.contains("recall"));
        assert!(md.contains("MRR"));
        assert!(md.contains("Zero-result rate"));
    }

    #[test]
    fn summary_markdown_includes_directional_label() {
        let report = make_test_report();
        let md = render_summary_markdown(&report);
        assert!(md.contains("Non-Gating"));
        assert!(md.contains("directional/non-gating"));
        assert!(md.contains("sparse"));
    }

    #[test]
    fn summary_markdown_includes_compare_failures() {
        let mut report = make_test_report();
        report.compare_result = Some(CompareResult {
            passed: false,
            failures: vec![crate::metrics::RegressionDetail {
                metric: "recall_at_1".to_string(),
                suite: "all_queries".to_string(),
                baseline_value: 0.8,
                current_value: 0.75,
                delta: -0.05,
                threshold: 0.02,
            }],
            query_regressions: vec![crate::metrics::QueryRegressionDetail {
                query_id: "q-001".to_string(),
                query_text: "How to handle race conditions?".to_string(),
                relevant_permalink: "cases/race".to_string(),
                old_rank: Some(1),
                new_rank: Some(5),
                metric_delta: -0.5,
            }],
        });
        let md = render_summary_markdown(&report);
        assert!(md.contains("FAILED"));
        assert!(md.contains("Per-Query Regression Details"));
        assert!(md.contains("q-001"));
        assert!(md.contains("cases/race"));
    }

    #[test]
    fn summary_markdown_includes_pass_status() {
        let mut report = make_test_report();
        report.compare_result = Some(CompareResult {
            passed: true,
            failures: vec![],
            query_regressions: vec![],
        });
        let md = render_summary_markdown(&report);
        assert!(md.contains("PASSED"));
    }

    #[test]
    fn baseline_round_trips_json() {
        let baseline = Phase1Baseline {
            metadata: BaselineMetadata {
                fixture_hashes: None,
                refresh_commit: "abc123".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
            },
            suite_metrics: HashMap::new(),
            aggregate_metrics: AggregateMetrics::default(),
            age_bucket_recall: HashMap::new(),
            per_query_ranks: HashMap::new(),
            signal_comparisons: vec![],
            threshold_policy_version: THRESHOLD_POLICY_VERSION.to_string(),
        };
        let json = serde_json::to_string_pretty(&baseline).unwrap();
        let parsed: Phase1Baseline = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.metadata.refresh_commit, "abc123");
        assert_eq!(parsed.threshold_policy_version, THRESHOLD_POLICY_VERSION);
    }

    #[test]
    fn write_and_load_baseline_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_root = tmp.path();

        let baseline = Phase1Baseline {
            metadata: BaselineMetadata {
                fixture_hashes: None,
                refresh_commit: "test-commit".to_string(),
                created_at: "2026-07-09T00:00:00Z".to_string(),
            },
            suite_metrics: {
                let mut m = HashMap::new();
                m.insert(
                    "all_queries".to_string(),
                    SuiteMetrics {
                        recall_at_1: 0.8,
                        recall_at_5: 0.9,
                        recall_at_10: 1.0,
                        mrr: 0.85,
                        zero_result_rate: 0.0,
                        query_count: 10,
                    },
                );
                m
            },
            aggregate_metrics: AggregateMetrics {
                recall_at_1: 0.8,
                recall_at_5: 0.9,
                recall_at_10: 1.0,
                mrr: 0.85,
                zero_result_rate: 0.0,
                query_count: 10,
            },
            age_bucket_recall: HashMap::new(),
            per_query_ranks: HashMap::new(),
            signal_comparisons: vec![],
            threshold_policy_version: THRESHOLD_POLICY_VERSION.to_string(),
        };

        write_baseline(crate_root, &baseline).unwrap();
        let loaded = load_baseline(crate_root).unwrap();
        assert_eq!(loaded.metadata.refresh_commit, "test-commit");
        assert!(loaded.suite_metrics.contains_key("all_queries"));
    }

    #[test]
    fn write_reports_creates_files() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().join("memory-eval");

        let report = make_test_report();
        write_reports(&report, &target_dir).unwrap();

        assert!(target_dir.join("phase1-report.json").exists());
        assert!(target_dir.join("phase1-summary.md").exists());

        let json_content = std::fs::read_to_string(target_dir.join("phase1-report.json")).unwrap();
        let parsed: Phase1Report = serde_json::from_str(&json_content).unwrap();
        assert_eq!(parsed.threshold_policy_version, THRESHOLD_POLICY_VERSION);
    }

    #[test]
    fn format_timestamp_produces_valid_iso8601() {
        // 2026-01-01T00:00:00Z = 1767225600
        let ts = format_timestamp(1767225600);
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
    }
}
