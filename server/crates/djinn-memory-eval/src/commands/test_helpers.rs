//! Test helpers for command-level validation tests.

use std::collections::HashMap;

use crate::fixtures::*;
use crate::metrics;
use crate::report::{self, BaselineMetadata, Phase1Baseline};
use crate::run;

/// Write a Phase1Fixtures to a temp directory structure on disk.
pub fn write_fixtures_to_disk(crate_root: &std::path::Path, fixtures: &Phase1Fixtures) {
    let fixtures_dir = crate_root.join(FixturePaths::FIXTURES_DIR);
    std::fs::create_dir_all(&fixtures_dir).unwrap();
    std::fs::write(
        fixtures_dir.join("corpus-notes.jsonl"),
        to_jsonl(&fixtures.corpus_notes).unwrap(),
    )
    .unwrap();
    std::fs::write(
        fixtures_dir.join("memory-ref-queries.jsonl"),
        to_jsonl(&fixtures.memory_ref_queries).unwrap(),
    )
    .unwrap();
    std::fs::write(
        fixtures_dir.join("bad-cases.jsonl"),
        to_jsonl(&fixtures.bad_cases).unwrap(),
    )
    .unwrap();
}

/// Write a Phase1Baseline to a temp directory on disk.
pub fn write_baseline_to_disk(crate_root: &std::path::Path, baseline: &Phase1Baseline) {
    let baselines_dir = crate_root.join(FixturePaths::BASELINES_DIR);
    std::fs::create_dir_all(&baselines_dir).unwrap();
    let json = serde_json::to_string_pretty(baseline).unwrap();
    std::fs::write(baselines_dir.join("phase1.json"), &json).unwrap();
}

/// Minimal corpus notes for testing.
pub fn minimal_corpus_notes() -> Vec<CorpusNoteRow> {
    vec![
        serde_json::from_str(
            r#"{"permalink":"notes/a","title":"Note A","content":"search","note_type":"case","folder":"notes","status":"active","tags":[],"timestamps":{"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z","last_accessed":"2026-06-01T00:00:00.000Z"},"confidence":0.8,"embedding":{"content_hash":"h1","model_version":"deterministic-v1","embedding_dim":3,"vector":[0.1,0.2,0.3]},"labels":[{"entity_type":"concept","name":"search"}],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":false}}"#,
        ).unwrap(),
        serde_json::from_str(
            r#"{"permalink":"notes/b","title":"Note B","content":"ranking","note_type":"pattern","folder":"notes","status":"active","tags":[],"timestamps":{"created_at":"2026-01-01T00:00:00.000Z","updated_at":"2026-01-01T00:00:00.000Z","last_accessed":"2026-06-01T00:00:00.000Z"},"confidence":0.8,"embedding":{"content_hash":"h2","model_version":"deterministic-v1","embedding_dim":3,"vector":[0.4,0.5,0.6]},"labels":[{"entity_type":"concept","name":"ranking"}],"graph_edges":[],"expected_signals":{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":false}}"#,
        ).unwrap(),
    ]
}

/// Generate N memory-ref query rows.
pub fn make_n_queries(n: usize) -> Vec<MinedMemoryRefRow> {
    (0..n)
        .map(|i| {
            serde_json::from_str(&format!(
                r#"{{"query_id":"q-{:03}","query_text":"q {}","task_id":null,"memory_refs":["notes/a"],"expected_signals":{{"vector":true,"lexical":true,"temporal":false,"graph":false,"entity":false,"task_affinity":false}}}}"#,
                i, i
            ))
            .unwrap()
        })
        .collect()
}

/// Generate N bad-case rows (alternating types for coverage).
pub fn make_n_bad_cases(n: usize) -> Vec<BadCaseRow> {
    let case_types = [
        BadCaseType::OverDecayThreshold,
        BadCaseType::GraphEntityInfluenced,
        BadCaseType::TaskAffinityInfluenced,
    ];
    let permalinks = ["notes/a", "notes/b", "notes/a"];
    (0..n)
        .map(|i| {
            let idx = i % case_types.len();
            BadCaseRow {
                case_id: format!("bc-{:03}", i),
                query_text: format!("bc {}", i),
                case_type: case_types[idx].clone(),
                expected_behavior: "test".to_string(),
                task_id: if case_types[idx] == BadCaseType::TaskAffinityInfluenced {
                    Some("test-task".to_string())
                } else {
                    None
                },
                relevant_note_permalinks: vec![permalinks[idx].to_string()],
                expected_signals: SignalCoverage {
                    vector: false,
                    lexical: true,
                    temporal: false,
                    graph: false,
                    entity: false,
                    task_affinity: false,
                },
                tags: vec![],
            }
        })
        .collect()
}

/// Make a baseline with explicit suite counts.
pub fn make_baseline_with_counts(all_queries: usize, bad_cases: usize) -> Phase1Baseline {
    let total = all_queries + bad_cases;
    let mut suite_metrics = HashMap::new();
    suite_metrics.insert(
        "all_queries".to_string(),
        metrics::SuiteMetrics {
            recall_at_1: 0.5, recall_at_5: 0.7, recall_at_10: 0.9,
            mrr: 0.6, zero_result_rate: 0.1, query_count: all_queries,
        },
    );
    suite_metrics.insert(
        "bad_cases".to_string(),
        metrics::SuiteMetrics {
            recall_at_1: 0.3, recall_at_5: 0.5, recall_at_10: 0.7,
            mrr: 0.4, zero_result_rate: 0.2, query_count: bad_cases,
        },
    );
    let mut per_query_ranks = HashMap::new();
    per_query_ranks.insert(
        "all_queries".to_string(),
        (0..all_queries)
            .map(|i| report::QueryRankBaseline {
                query_id: format!("q-{:03}", i), query_text: format!("q {}", i),
                result_permalinks: vec!["notes/a".to_string()],
                relevant_ranks: vec![Some(1)], best_rank: Some(1),
            })
            .collect(),
    );
    per_query_ranks.insert(
        "bad_cases".to_string(),
        (0..bad_cases)
            .map(|i| report::QueryRankBaseline {
                query_id: format!("bc-{:03}", i), query_text: format!("bc {}", i),
                result_permalinks: vec!["notes/a".to_string()],
                relevant_ranks: vec![Some(1)], best_rank: Some(1),
            })
            .collect(),
    );
    Phase1Baseline {
        metadata: BaselineMetadata {
            fixture_hashes: None,
            refresh_commit: "test-commit-sha".to_string(),
            created_at: "2026-07-10T00:00:00Z".to_string(),
        },
        suite_metrics,
        aggregate_metrics: metrics::AggregateMetrics {
            recall_at_1: 0.5, recall_at_5: 0.7, recall_at_10: 0.9,
            mrr: 0.6, zero_result_rate: 0.1, query_count: total,
        },
        age_bucket_recall: HashMap::new(),
        per_query_ranks,
        signal_comparisons: vec![
            run::SignalRankComparison {
                query_id: "q-000".to_string(), signal: "graph".to_string(),
                rank_with_signal: Some(1), rank_without_signal: Some(5), rank_changed: true,
            },
            run::SignalRankComparison {
                query_id: "q-001".to_string(), signal: "task_affinity".to_string(),
                rank_with_signal: Some(1), rank_without_signal: Some(5), rank_changed: true,
            },
        ],
        threshold_policy_version: metrics::THRESHOLD_POLICY_VERSION.to_string(),
    }
}

/// Build a baseline with explicit signal_comparisons.
pub fn make_baseline_with_signal_comparisons(
    signal_comparisons: Vec<run::SignalRankComparison>,
) -> Phase1Baseline {
    let mut baseline = make_baseline_with_counts(20, 10);
    baseline.signal_comparisons = signal_comparisons;
    baseline
}
