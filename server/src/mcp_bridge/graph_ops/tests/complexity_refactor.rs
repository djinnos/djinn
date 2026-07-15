use super::*;

fn complexity_metrics(
    cog: u16,
    cyc: u16,
    nloc: u16,
    nest: u8,
    params: u8,
) -> WireComplexityMetrics {
    WireComplexityMetrics {
        cyclomatic: cyc,
        cognitive: cog,
        nloc,
        max_nesting: nest,
        param_count: params,
    }
}

fn function_entry(
    key: &str,
    display_name: &str,
    file: &str,
    metrics: WireComplexityMetrics,
) -> djinn_control_plane::bridge::FunctionComplexityEntry {
    djinn_control_plane::bridge::FunctionComplexityEntry {
        key: key.to_string(),
        display_name: display_name.to_string(),
        file: file.to_string(),
        start_line: 1,
        end_line: 10,
        metrics,
    }
}

#[test]
fn complexity_sort_functions_by_cognitive_iter28() {
    // Two functions in two files — one with cognitive=10, one with
    // cognitive=1. After sorting by cognitive desc, the high-
    // complexity entry must lead.
    let mut entries = vec![
        function_entry(
            "symbol:a",
            "easy",
            "src/a.rs",
            complexity_metrics(1, 1, 5, 0, 0),
        ),
        function_entry(
            "symbol:b",
            "hard",
            "src/b.rs",
            complexity_metrics(10, 8, 50, 4, 3),
        ),
    ];
    super::refactor::sort_function_complexity_entries(&mut entries, "cognitive");
    assert_eq!(entries[0].display_name, "hard");
    assert_eq!(entries[0].metrics.cognitive, 10);
    assert_eq!(entries[1].display_name, "easy");
    assert_eq!(entries[1].metrics.cognitive, 1);
}

#[test]
fn complexity_sort_functions_by_cyclomatic_iter28() {
    // Verify the non-default sort key actually rotates the ordering.
    // `easy` has higher cognitive but lower cyclomatic.
    let mut entries = vec![
        function_entry(
            "symbol:easy",
            "easy",
            "src/a.rs",
            complexity_metrics(10, 2, 5, 0, 0),
        ),
        function_entry(
            "symbol:hard",
            "hard",
            "src/b.rs",
            complexity_metrics(5, 9, 50, 4, 3),
        ),
    ];
    super::refactor::sort_function_complexity_entries(&mut entries, "cyclomatic");
    assert_eq!(
        entries[0].display_name, "hard",
        "cyclomatic=9 should win over cyclomatic=2"
    );
    assert_eq!(entries[0].metrics.cyclomatic, 9);
}

#[test]
fn complexity_aggregate_files_groups_by_path_iter28() {
    // Two functions in `src/big.rs` (cognitive 7+3) and one in
    // `src/small.rs` (cognitive 2). After aggregation: big.rs has
    // function_count=2, total_cognitive=10, max_function_name="big_fn"
    // (worst offender); small.rs has 1 function. Sorted by total
    // cognitive desc, big.rs leads.
    let entries = vec![
        function_entry(
            "symbol:big1",
            "big_fn",
            "src/big.rs",
            complexity_metrics(7, 5, 30, 2, 2),
        ),
        function_entry(
            "symbol:big2",
            "small_fn",
            "src/big.rs",
            complexity_metrics(3, 2, 12, 1, 1),
        ),
        function_entry(
            "symbol:s1",
            "tiny",
            "src/small.rs",
            complexity_metrics(2, 1, 8, 0, 0),
        ),
    ];
    let files = super::refactor::aggregate_files_complexity(&entries, "cognitive");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].file, "src/big.rs");
    assert_eq!(files[0].function_count, 2);
    assert_eq!(files[0].total_cognitive, 10);
    assert_eq!(files[0].total_cyclomatic, 7);
    assert_eq!(files[0].total_nloc, 42);
    assert_eq!(files[0].max_function_cognitive, 7);
    assert_eq!(files[0].max_function_name, "big_fn");
    assert_eq!(files[1].file, "src/small.rs");
    assert_eq!(files[1].function_count, 1);
}

#[test]
fn complexity_aggregate_files_param_count_proxy_iter28() {
    // For `target=files, sort_by=param_count` we proxy to
    // function_count (formal params don't sum meaningfully across
    // functions). A file with one 5-param function ranks BELOW a
    // file with two 1-param functions.
    let entries = vec![
        function_entry(
            "symbol:a",
            "single",
            "src/wide.rs",
            complexity_metrics(1, 1, 5, 0, 5),
        ),
        function_entry(
            "symbol:b",
            "first",
            "src/many.rs",
            complexity_metrics(1, 1, 5, 0, 1),
        ),
        function_entry(
            "symbol:c",
            "second",
            "src/many.rs",
            complexity_metrics(1, 1, 5, 0, 1),
        ),
    ];
    let files = super::refactor::aggregate_files_complexity(&entries, "param_count");
    assert_eq!(
        files[0].file, "src/many.rs",
        "two functions should win by param_count proxy"
    );
    assert_eq!(files[0].function_count, 2);
    assert_eq!(files[1].file, "src/wide.rs");
    assert_eq!(files[1].function_count, 1);
}

#[test]
fn complexity_result_serializes_as_array_iter28() {
    // Serde-untagged invariant: a `Functions` variant serializes as
    // a bare JSON array (the inner Vec). Pinning this so a future
    // refactor that wraps it in a discriminator breaks the test
    // explicitly.
    use djinn_control_plane::bridge::ComplexityResult;
    let entry = function_entry(
        "symbol:x",
        "x",
        "src/x.rs",
        complexity_metrics(1, 1, 1, 0, 0),
    );
    let result = ComplexityResult::Functions(vec![entry]);
    let json = serde_json::to_value(&result).expect("serialize");
    assert!(
        json.is_array(),
        "Functions should serialize as bare array: {json}"
    );
    assert_eq!(json.as_array().unwrap().len(), 1);
}

// ── Iter 29: refactor_candidates composite ranking ────────────────────

fn refactor_input(
    key: &str,
    display_name: &str,
    file: &str,
    cognitive: u16,
    cyclomatic: u16,
    page_rank: f64,
) -> super::refactor::RefactorCandidateInput {
    super::refactor::RefactorCandidateInput {
        key: key.to_string(),
        display_name: display_name.to_string(),
        file: file.to_string(),
        start_line: 1,
        end_line: 10,
        cognitive,
        cyclomatic,
        page_rank,
        cross_module_cochange: 0.0,
    }
}

#[test]
fn refactor_candidates_composite_ranks_top_function_iter29() {
    // Three functions with monotonically-increasing signals across
    // all three axes. Function B (cognitive=10, churn=20, pr=0.5)
    // tops every signal AND the composite z-score; the ranker
    // must surface it at index 0.
    use std::collections::HashMap;
    let candidates = vec![
        refactor_input("symbol:a", "a", "src/a.rs", 1, 1, 0.1),
        refactor_input("symbol:b", "b", "src/b.rs", 10, 8, 0.5),
        refactor_input("symbol:c", "c", "src/c.rs", 5, 4, 0.2),
    ];
    let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
    churn_map.insert(std::path::PathBuf::from("src/a.rs"), 1);
    churn_map.insert(std::path::PathBuf::from("src/b.rs"), 20);
    churn_map.insert(std::path::PathBuf::from("src/c.rs"), 5);

    let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
    assert_eq!(out.len(), 3);
    assert_eq!(
        out[0].display_name, "b",
        "B should be the top refactor target"
    );
    assert_eq!(out[0].cognitive, 10);
    assert_eq!(out[0].churn_commits, 20);
    // Score is the mean of three z-scores; with B at the top of
    // every axis the composite must be strictly positive.
    assert!(out[0].composite_score > 0.0, "B composite should be > 0");
}

#[test]
fn refactor_candidates_cross_module_cochange_raises_rank_qoxm() {
    // Proposal qoxm: two functions identical on complexity / churn /
    // pagerank, but one lives in a config file that co-changes with a source
    // file in ANOTHER crate. The cross-module co-change axis must break the tie
    // in favor of the coupled config file — the exact hidden-coupling signal
    // static analysis misses.
    use std::collections::HashMap;
    let config = super::refactor::RefactorCandidateInput {
        key: "symbol:config".to_string(),
        display_name: "load_config".to_string(),
        file: "crates/app_config/src/settings.rs".to_string(),
        start_line: 1,
        end_line: 10,
        cognitive: 5,
        cyclomatic: 3,
        page_rank: 0.2,
        // Strongly co-changes with a source file in a different crate.
        cross_module_cochange: 0.8,
    };
    let plain = super::refactor::RefactorCandidateInput {
        key: "symbol:plain".to_string(),
        display_name: "plain_helper".to_string(),
        file: "crates/app_core/src/util.rs".to_string(),
        start_line: 1,
        end_line: 10,
        cognitive: 5,
        cyclomatic: 3,
        page_rank: 0.2,
        cross_module_cochange: 0.0,
    };
    let candidates = vec![plain, config];
    let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
    churn_map.insert(
        std::path::PathBuf::from("crates/app_config/src/settings.rs"),
        4,
    );
    churn_map.insert(std::path::PathBuf::from("crates/app_core/src/util.rs"), 4);

    let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out[0].display_name, "load_config",
        "the cross-module co-changed config file must rank first"
    );
    assert!(
        out[0].composite_score > out[1].composite_score,
        "co-change pressure must raise the composite above the identical-but-uncoupled peer"
    );
}

#[test]
fn module_key_distinguishes_crates_qoxm() {
    assert_eq!(
        super::refactor::module_key("crates/app_config/src/settings.rs"),
        "app_config"
    );
    assert_eq!(
        super::refactor::module_key("server/crates/djinn-graph/src/lib.rs"),
        "djinn-graph"
    );
    assert_eq!(super::refactor::module_key("ui/src/main.ts"), "ui");
    assert_ne!(
        super::refactor::module_key("crates/a/src/x.rs"),
        super::refactor::module_key("crates/b/src/y.rs")
    );
}

#[test]
fn refactor_candidates_zero_stddev_returns_zero_z_iter29() {
    // Degenerate small-project shape: every function has the same
    // cognitive / churn / pagerank. Stddev across each axis is 0;
    // the helper must clamp z-scores to 0 (not produce NaN), and
    // the composite score for every entry must be exactly 0.
    // Order is stable on the display_name tiebreaker.
    use std::collections::HashMap;
    let candidates = vec![
        refactor_input("symbol:a", "alpha", "src/x.rs", 5, 3, 0.2),
        refactor_input("symbol:b", "beta", "src/x.rs", 5, 3, 0.2),
        refactor_input("symbol:c", "gamma", "src/x.rs", 5, 3, 0.2),
    ];
    let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
    churn_map.insert(std::path::PathBuf::from("src/x.rs"), 7);

    let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
    assert_eq!(out.len(), 3);
    for entry in &out {
        assert_eq!(
            entry.composite_score, 0.0,
            "composite should be 0: {entry:?}"
        );
        assert_eq!(entry.z_cognitive, 0.0);
        assert_eq!(entry.z_churn, 0.0);
        assert_eq!(entry.z_page_rank, 0.0);
    }
    // Stable order: alphabetical by display_name on the
    // composite-score tie.
    assert_eq!(out[0].display_name, "alpha");
    assert_eq!(out[1].display_name, "beta");
    assert_eq!(out[2].display_name, "gamma");
}

#[test]
fn refactor_candidates_tier_assignment_iter29() {
    // Build 20 candidates with monotonically-increasing cognitive +
    // churn so the composite ranks them in the same order. After
    // sorting:
    //   - 10% × 20 = 2 entries get tier="high"
    //   - 15% × 20 = 3 entries get tier="medium"
    //   - the remaining 15 get tier="low"
    use std::collections::HashMap;
    let mut candidates = Vec::new();
    let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
    for i in 0..20 {
        // Higher i → higher cognitive AND higher churn → higher composite.
        let key = format!("symbol:{i:02}");
        let display = format!("fn_{i:02}");
        let file = format!("src/f{i:02}.rs");
        candidates.push(refactor_input(
            &key,
            &display,
            &file,
            u16::try_from(i + 1).unwrap(),
            1,
            f64::from(i),
        ));
        churn_map.insert(
            std::path::PathBuf::from(&file),
            u32::try_from(i + 1).unwrap(),
        );
    }
    let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 20);
    assert_eq!(out.len(), 20);
    let high_count = out.iter().filter(|c| c.tier == "high").count();
    let medium_count = out.iter().filter(|c| c.tier == "medium").count();
    let low_count = out.iter().filter(|c| c.tier == "low").count();
    assert_eq!(high_count, 2, "10% of 20 = 2 high");
    assert_eq!(medium_count, 3, "15% of 20 = 3 medium");
    assert_eq!(low_count, 15, "rest are low");
    // Top entries are the high tier; bottom entries are low.
    assert_eq!(out[0].tier, "high");
    assert_eq!(out[1].tier, "high");
    assert_eq!(out[2].tier, "medium");
    assert_eq!(out[3].tier, "medium");
    assert_eq!(out[4].tier, "medium");
    assert_eq!(out[5].tier, "low");
    assert_eq!(out[19].tier, "low");
}

#[test]
fn refactor_candidates_small_set_all_high_iter29() {
    // Sets with fewer than 10 candidates collapse to all-high
    // (degenerate small project). The 10/15/75 split needs enough
    // entries for the rounding to be meaningful.
    use std::collections::HashMap;
    let candidates = vec![
        refactor_input("symbol:a", "a", "src/a.rs", 5, 3, 0.2),
        refactor_input("symbol:b", "b", "src/b.rs", 8, 4, 0.3),
    ];
    let churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();

    let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].tier, "high");
    assert_eq!(out[1].tier, "high");
}

#[test]
fn refactor_candidates_missing_file_in_churn_yields_zero_iter29() {
    // Spec: a function whose file isn't in the churn map gets
    // churn_commits=0 (not skipped). With one function in the map
    // (high churn) and one absent (zero churn), the absent
    // function gets a negative z_churn — correct, "this file
    // changes less than average".
    use std::collections::HashMap;
    let candidates = vec![
        refactor_input("symbol:a", "a", "src/in_map.rs", 5, 3, 0.2),
        refactor_input("symbol:b", "b", "src/missing.rs", 5, 3, 0.2),
    ];
    let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
    churn_map.insert(std::path::PathBuf::from("src/in_map.rs"), 50);

    let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 30);
    assert_eq!(out.len(), 2);
    // The missing-file function inherits churn_commits=0.
    let missing = out.iter().find(|c| c.display_name == "b").unwrap();
    assert_eq!(missing.churn_commits, 0);
    assert!(
        missing.z_churn < 0.0,
        "absent file should have negative z_churn"
    );
    // The in-map function has positive z_churn.
    let present = out.iter().find(|c| c.display_name == "a").unwrap();
    assert_eq!(present.churn_commits, 50);
    assert!(
        present.z_churn > 0.0,
        "high-churn file should have positive z_churn"
    );
}

#[test]
fn refactor_candidates_empty_input_returns_empty_iter29() {
    // No candidates → empty Vec (success, not error). Caller must
    // tolerate empty results without a special-case branch.
    use std::collections::HashMap;
    let out = super::refactor::compute_refactor_candidates(&[], &HashMap::new(), 30);
    assert!(out.is_empty());
}

#[test]
fn refactor_candidates_truncates_to_limit_iter29() {
    // Limit caps the returned set; the surviving entries are the
    // top-`limit` by composite score.
    use std::collections::HashMap;
    let mut candidates = Vec::new();
    let mut churn_map: HashMap<std::path::PathBuf, u32> = HashMap::new();
    for i in 0..50 {
        let key = format!("symbol:{i:02}");
        let display = format!("fn_{i:02}");
        let file = format!("src/f{i:02}.rs");
        candidates.push(refactor_input(
            &key,
            &display,
            &file,
            u16::try_from(i + 1).unwrap(),
            1,
            f64::from(i),
        ));
        churn_map.insert(
            std::path::PathBuf::from(&file),
            u32::try_from(i + 1).unwrap(),
        );
    }
    let out = super::refactor::compute_refactor_candidates(&candidates, &churn_map, 5);
    assert_eq!(out.len(), 5);
    // Top entry is the highest-index candidate (largest signals).
    assert_eq!(out[0].display_name, "fn_49");
}
