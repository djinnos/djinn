use super::graph_queries::{definition_with_enclosing, nested_ranges_fixture};
use super::*;

#[test]
fn symbols_enclosing_range_inside_single_symbol_returns_only_that_symbol() {
    let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
    // Lines 27..=28 fall wholly inside `sibling_fn` (26..=30).
    let hits = graph.symbols_enclosing(Path::new("src/lib.rs"), 27, 28);
    let names: Vec<_> = hits
        .iter()
        .map(|idx| graph.node(*idx).display_name.as_str())
        .collect();
    assert_eq!(names, vec!["sibling_fn"]);
}

#[test]
fn symbols_enclosing_range_crossing_sibling_symbols_returns_both() {
    let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
    // Lines 20..=26 span the gap between `outer` (1..=20) and
    // `sibling_fn` (26..=30); both overlap at their boundary lines.
    let hits = graph.symbols_enclosing(Path::new("src/lib.rs"), 20, 26);
    let mut names: Vec<_> = hits
        .iter()
        .map(|idx| graph.node(*idx).display_name.clone())
        .collect();
    names.sort();
    assert_eq!(names, vec!["outer".to_string(), "sibling_fn".to_string()]);
}

#[test]
fn symbols_enclosing_nested_symbols_all_enclose_query() {
    let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
    // Line 9 sits inside `inner_method` (8..=10), which is inside
    // `Inner` (6..=12), which is inside `outer` (1..=20).
    let hits = graph.symbols_enclosing(Path::new("src/lib.rs"), 9, 9);
    let mut names: Vec<_> = hits
        .iter()
        .map(|idx| graph.node(*idx).display_name.clone())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "Inner".to_string(),
            "inner_method".to_string(),
            "outer".to_string()
        ]
    );
}

#[test]
fn symbols_enclosing_file_without_ranges_returns_empty() {
    // The base fixture uses definition_occurrence() which has
    // enclosing_range=None, so symbol_ranges is empty for that file.
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let hits = graph.symbols_enclosing(Path::new("src/app.rs"), 1, 100);
    assert!(hits.is_empty());
}

#[test]
fn symbols_enclosing_unknown_file_returns_empty() {
    let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
    let hits = graph.symbols_enclosing(Path::new("src/does_not_exist.rs"), 1, 10);
    assert!(hits.is_empty());
}

#[test]
fn symbols_enclosing_round_trips_through_artifact() {
    // PR A1: `symbol_ranges` must be persisted in the artifact so that
    // `code_graph symbols_at` / `diff_touches` keep working after a
    // cache-hit reload (DB-restored graph).
    let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
    let baseline = graph.symbols_enclosing(Path::new("src/lib.rs"), 9, 9);
    assert!(
        !baseline.is_empty(),
        "fixture must produce ranges before round-trip"
    );

    let artifact = graph.to_artifact();
    assert!(
        !artifact.symbol_ranges.is_empty(),
        "artifact must carry symbol_ranges"
    );
    let restored = RepoDependencyGraph::from_artifact(&artifact);

    let hits = restored.symbols_enclosing(Path::new("src/lib.rs"), 9, 9);
    assert!(
        !hits.is_empty(),
        "artifact-restored graph must preserve enclosing-range hits"
    );

    let mut names: Vec<_> = hits
        .iter()
        .map(|idx| restored.node(*idx).display_name.clone())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "Inner".to_string(),
            "inner_method".to_string(),
            "outer".to_string()
        ],
        "restored ranges must match the freshly-built graph's hits"
    );
}

#[test]
fn symbol_ranges_round_trip_through_json_artifact() {
    // Belt-and-suspenders coverage of the JSON path used by
    // `serialize_artifact` / `deserialize_artifact`, which is what the
    // cache-hit reload exercises end-to-end.
    let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
    let json = graph.serialize_artifact().expect("serialize");
    let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize");
    let hits = restored.symbols_enclosing(Path::new("src/other.rs"), 1, 5);
    assert!(
        !hits.is_empty(),
        "JSON-round-tripped graph must preserve symbol_ranges"
    );
}

// ── iter 26: complexity metrics post-pass ─────────────────────────────

/// Build a Rust SCIP fixture with two function symbols whose enclosing
/// ranges line up with `source` so the post-build complexity walker
/// can pair them by overlap.
fn complexity_fixture(source: &str) -> ParsedScipIndex {
    // Both `simple` and `nested` start at the top of file in `source`;
    // we hard-code the ranges to match the literal layout below.
    // `simple`: lines 1..=4 (1-indexed inclusive after normalization),
    // `nested`: lines 6..=15.
    let simple_sym = ScipSymbol {
        symbol: "scip-rust pkg src/lib.rs `simple`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("simple".to_string()),
        signature: Some("fn simple()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let nested_sym = ScipSymbol {
        symbol: "scip-rust pkg src/lib.rs `nested`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("nested".to_string()),
        signature: Some("fn nested(a: i32, b: i32)".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    // `definition_with_enclosing` takes 0-indexed wire ranges and the
    // builder bumps them to 1-indexed inclusive — see
    // `record_symbol_range`. So a fn whose body spans 1-indexed
    // inclusive lines [1,4] is encoded as (0, 3) here.
    let _ = source; // silence unused if the layout drifts; lines pinned below.

    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("scip-rust".to_string()),
            tool_version: Some("test".to_string()),
        },
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("src/lib.rs"),
            definitions: vec![
                definition_with_enclosing(&simple_sym.symbol, 0, 2),
                definition_with_enclosing(&nested_sym.symbol, 4, 13),
            ],
            references: vec![],
            occurrences: vec![],
            symbols: vec![simple_sym, nested_sym],
        }],
        external_symbols: vec![],
    }
}

/// Source whose tree-sitter ranges align with `complexity_fixture`:
///   `simple`: 0-indexed rows 0..=2  (1-indexed 1..=3)
///   `nested`: 0-indexed rows 4..=13 (1-indexed 5..=14)
/// The body of `nested` carries an `if` inside an `if` inside a `for`
/// inside an `if`: cognitive = 1 + 2 + 3 + 4 = 10
/// (matches `complexity::tests::deeply_nested_chains_correctly`).
const COMPLEXITY_FIXTURE_SOURCE: &str = "fn simple() {\n    let _ = 1;\n}\n\nfn nested(a: i32, b: i32) {\n    if a > 0 {\n        if b > 0 {\n            for _ in 0..a {\n                if b == 1 {\n                }\n            }\n        }\n    }\n}\n";

#[test]
fn build_with_source_attaches_complexity_to_function_nodes() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let abs = tempdir.path().join("src/lib.rs");
    std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    std::fs::write(&abs, COMPLEXITY_FIXTURE_SOURCE).expect("write");

    let graph = RepoDependencyGraph::build_with_source(
        &[complexity_fixture(COMPLEXITY_FIXTURE_SOURCE)],
        Some(tempdir.path()),
    );

    let simple_idx = graph
        .symbol_node("scip-rust pkg src/lib.rs `simple`().")
        .expect("simple node");
    let nested_idx = graph
        .symbol_node("scip-rust pkg src/lib.rs `nested`().")
        .expect("nested node");

    let simple = graph.node(simple_idx);
    let nested = graph.node(nested_idx);

    let simple_metrics = simple.complexity.expect("simple has metrics");
    assert_eq!(simple_metrics.cyclomatic, 1);
    assert_eq!(simple_metrics.cognitive, 0);

    let nested_metrics = nested.complexity.expect("nested has metrics");
    assert_eq!(nested_metrics.cognitive, 1 + 2 + 3 + 4);
    assert_eq!(nested_metrics.param_count, 2);
    assert_eq!(nested_metrics.max_nesting, 4);
}

/// scip-typescript regression: it emits `SymbolInformation.kind = 0`
/// (UnspecifiedKind) for every symbol, so kind-based function detection
/// rejects ALL TypeScript symbols and complexity silently never attaches.
/// The fallback must recognize the SCIP method descriptor suffix `")."`.
#[test]
fn build_with_source_attaches_complexity_despite_unknown_symbol_kind() {
    const TS_SOURCE: &str = "function pick(a: number): number {\n    if (a > 0) {\n        return 1;\n    }\n    return 0;\n}\n";

    let sym = ScipSymbol {
        symbol: "scip-typescript npm pkg 1.0.0 src/`pick.ts`/pick().".to_string(),
        // scip-typescript: kind is the proto default, parsed as Unknown(0).
        kind: Some(ScipSymbolKind::Unknown(0)),
        display_name: Some("pick".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "ui".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo/ui".to_string()),
            tool_name: Some("scip-typescript".to_string()),
            tool_version: Some("test".to_string()),
        },
        files: vec![ScipFile {
            language: "typescript".to_string(),
            relative_path: PathBuf::from("ui/src/pick.ts"),
            definitions: vec![definition_with_enclosing(&sym.symbol, 0, 5)],
            references: vec![],
            occurrences: vec![],
            symbols: vec![sym],
        }],
        external_symbols: vec![],
    };

    let tempdir = tempfile::tempdir().expect("tempdir");
    let abs = tempdir.path().join("ui/src/pick.ts");
    std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    std::fs::write(&abs, TS_SOURCE).expect("write");

    let graph = RepoDependencyGraph::build_with_source(&[index], Some(tempdir.path()));
    let idx = graph
        .symbol_node("scip-typescript npm pkg 1.0.0 src/`pick.ts`/pick().")
        .expect("pick node");
    let metrics = graph
        .node(idx)
        .complexity
        .expect("Unknown-kind function symbol must still get complexity");
    assert_eq!(metrics.cognitive, 1);
    assert_eq!(metrics.cyclomatic, 2);
}

#[test]
fn build_without_source_leaves_complexity_unset() {
    // `build` (no project_root) is the synthetic-fixture path used by
    // most unit tests in this file. The post-pass must short-circuit
    // gracefully — no panic, no metrics, just `None` everywhere.
    let graph = RepoDependencyGraph::build(&[complexity_fixture(COMPLEXITY_FIXTURE_SOURCE)]);
    let simple_idx = graph
        .symbol_node("scip-rust pkg src/lib.rs `simple`().")
        .expect("simple node");
    assert!(graph.node(simple_idx).complexity.is_none());
}

#[test]
fn complexity_round_trips_through_artifact() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let abs = tempdir.path().join("src/lib.rs");
    std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    std::fs::write(&abs, COMPLEXITY_FIXTURE_SOURCE).expect("write");

    let graph = RepoDependencyGraph::build_with_source(
        &[complexity_fixture(COMPLEXITY_FIXTURE_SOURCE)],
        Some(tempdir.path()),
    );

    let artifact = graph.to_artifact();
    assert_eq!(artifact.version, REPO_GRAPH_ARTIFACT_VERSION);

    let restored = RepoDependencyGraph::from_artifact(&artifact);
    let nested_idx = restored
        .symbol_node("scip-rust pkg src/lib.rs `nested`().")
        .expect("nested node restored");
    let metrics = restored
        .node(nested_idx)
        .complexity
        .expect("metrics survive round-trip");
    assert_eq!(metrics.cognitive, 1 + 2 + 3 + 4);
    assert_eq!(metrics.param_count, 2);
}

#[test]
fn complexity_skips_files_with_unsupported_language() {
    // SCIP `Document.language` strings outside the walker's table
    // (iter 23–25 ships 11 languages — anything else, like
    // "haskell", falls through `ComplexityLang::from_scip`). The
    // post-pass must skip silently rather than panic, leaving
    // `complexity = None` on every node.
    let hs_source = "module M where\nf :: Int\nf = 0\n";
    let tempdir = tempfile::tempdir().expect("tempdir");
    let abs = tempdir.path().join("src/M.hs");
    std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    std::fs::write(&abs, hs_source).expect("write");

    let hs_sym = ScipSymbol {
        symbol: "scip-haskell pkg src/M.hs `f`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("f".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![ScipFile {
            language: "haskell".to_string(),
            relative_path: PathBuf::from("src/M.hs"),
            definitions: vec![definition_with_enclosing(&hs_sym.symbol, 2, 3)],
            references: vec![],
            occurrences: vec![],
            symbols: vec![hs_sym],
        }],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build_with_source(&[index], Some(tempdir.path()));
    let f_idx = graph
        .symbol_node("scip-haskell pkg src/M.hs `f`().")
        .expect("haskell fn node");
    assert!(graph.node(f_idx).complexity.is_none());
}
