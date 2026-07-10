
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

/// An over-decay note referenced by a bad-case record must contribute
/// to the over_decay_threshold age bucket in age-bucket recall curves.
/// This test verifies that bad-case records (not just memory-ref queries)
/// are included when computing age-bucket recall.
#[test]
fn over_decay_bad_case_contributes_to_age_bucket_recall() {
    // Simulate a bad-case record for the over-decay fixture
    let bad_record = QueryRankRecord {
        query_id: "bc-over-decay-001".to_string(),
        query_text: "slot cold start failure".to_string(),
        task_id: None,
        result_permalinks: vec!["cases/over-decay-slot-setup".to_string()],
        relevant_ranks: vec![Some(1)],
        expected_permalinks: vec!["cases/over-decay-slot-setup".to_string()],
        is_bad_case: true,
        bad_case_type: Some(BadCaseType::OverDecayThreshold),
    };

    // Also include a fresh note from a memory-ref query
    let query_record = make_record("q-001", vec!["fresh-note"], vec![Some(1)], false);

    let all_records = vec![query_record, bad_record];
    let mut note_ages = HashMap::new();
    note_ages.insert("fresh-note".to_string(), 3); // <7d
    note_ages.insert("cases/over-decay-slot-setup".to_string(), 200); // >90d

    let buckets = compute_age_bucket_recall(&all_records, &note_ages);

    // The over-decay bucket MUST be present
    assert!(
        buckets.contains_key(&AgeBucket::OverDecayThreshold),
        "over_decay_threshold bucket must be present when bad-case record \
             references a note older than 90 days"
    );

    let over_decay = buckets.get(&AgeBucket::OverDecayThreshold).unwrap();
    assert!(
        (over_decay.recall_at_1 - 1.0).abs() < 1e-10,
        "over-decay note found at rank 1 should have recall@1 = 1.0"
    );

    // The fresh bucket should also be present
    assert!(buckets.contains_key(&AgeBucket::Under7d));
}
