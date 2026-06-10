// Test module for the `repo_graph` submodule.
//
// Pulled verbatim out of the original 4,573-line `repo_graph.rs` (lines
// 2519-4573) and dedented to live at the top level of this file. The
// parent `repo_graph/mod.rs` declares this as
// `#[cfg(test)] mod tests;` so the test path remains `repo_graph::tests`
// and the inner `use super::*;` continues to resolve through the
// re-exports in `mod.rs`.
//
// No logic changes — the tests are byte-for-byte identical to the
// pre-split tree (only the indentation and `mod tests {` wrapper
// have been removed).

use std::path::PathBuf;

use petgraph::visit::EdgeRef;
use serde::Serialize;

use super::*;
use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
    ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
};

#[test]
fn builds_dependency_graph_with_file_and_symbol_metadata() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);

    // PR F2 bumped the count from 5 to 6: the entry-point detector
    // tags `main` as `EntryPointKind::Main`, and the process
    // detector traces a (short) call chain from it, materializing
    // one synthetic `Process` node. The five SCIP-derived nodes
    // (2 files + 3 symbols) are still all present; the extra node
    // is the synthetic process.
    assert_eq!(graph.node_count(), 6);
    assert!(graph.edge_count() >= 8);

    let app_file = graph
        .file_node("src/app.rs")
        .expect("app file node should exist");
    let app_node = graph.node(app_file);
    assert_eq!(app_node.kind, RepoGraphNodeKind::File);
    assert_eq!(app_node.language.as_deref(), Some("rust"));
    assert_eq!(app_node.file_path.as_deref(), Some(Path::new("src/app.rs")));
    assert_eq!(app_node.workspace.as_deref(), Some("root"));

    let helper_symbol = graph
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper symbol node should exist");
    let helper_node = graph.node(helper_symbol);
    assert_eq!(helper_node.kind, RepoGraphNodeKind::Symbol);
    assert_eq!(helper_node.symbol_kind, Some(ScipSymbolKind::Function));
    assert_eq!(
        helper_node.file_path.as_deref(),
        Some(Path::new("src/helper.rs"))
    );
    assert_eq!(helper_node.workspace.as_deref(), Some("root"));

    let has_file_reference = graph.graph().edges(app_file).any(|edge| {
        edge.target() == helper_symbol && edge.weight().kind == RepoGraphEdgeKind::FileReference
    });
    assert!(has_file_reference, "expected file->symbol reference edge");
}

/// v10: the build stamps `is_test` on File nodes (and the symbols
/// defined in them) by the file-path convention, while leaving
/// production files/symbols unmarked.
#[test]
fn build_stamps_is_test_from_file_path_convention() {
    let test_symbol_name = "scip-rust pkg tests/login_test.rs `it_logs_in`().".to_string();
    let test_symbol = ScipSymbol {
        symbol: test_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("it_logs_in".to_string()),
        signature: Some("fn it_logs_in()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let prod_symbol_name = "scip-rust pkg src/login.rs `login`().".to_string();
    let prod_symbol = ScipSymbol {
        symbol: prod_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("login".to_string()),
        signature: Some("fn login()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("tests/login_test.rs"),
                definitions: vec![definition_occurrence(&test_symbol_name)],
                references: vec![],
                occurrences: vec![definition_occurrence(&test_symbol_name)],
                symbols: vec![test_symbol],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/login.rs"),
                definitions: vec![definition_occurrence(&prod_symbol_name)],
                references: vec![],
                occurrences: vec![definition_occurrence(&prod_symbol_name)],
                symbols: vec![prod_symbol],
            },
        ],
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[index]);

    let test_file = graph
        .file_node("tests/login_test.rs")
        .expect("test file node should exist");
    assert!(
        graph.node(test_file).is_test,
        "file under tests/ must be marked is_test"
    );
    let test_sym = graph
        .symbol_node(&test_symbol_name)
        .expect("test symbol node should exist");
    assert!(
        graph.node(test_sym).is_test,
        "symbol defined in a test file must inherit is_test"
    );

    let prod_file = graph
        .file_node("src/login.rs")
        .expect("prod file node should exist");
    assert!(
        !graph.node(prod_file).is_test,
        "production file must not be marked is_test"
    );
    let prod_sym = graph
        .symbol_node(&prod_symbol_name)
        .expect("prod symbol node should exist");
    assert!(
        !graph.node(prod_sym).is_test,
        "production symbol must not be marked is_test"
    );
}

/// Regression test for the sparse PageRank replacement.  Validates
/// the three properties the downstream ranking code depends on:
///
/// 1. Output length matches node count (required by indexing in
///    `rank()`).
/// 2. All ranks are finite, non-negative, and sum to ~1 (mass
///    preservation + normalization — a sanity check against FP
///    drift or dangling-node mass loss).
/// 3. Isolated nodes (no in-edges, no out-edges) share the same
///    rank — they receive only the random-jump + dangling baseline.
///
/// Does NOT assert numerical equivalence with petgraph's 0.8.3
/// `page_rank` — that implementation uses a different per-pair
/// formulation, so direct comparison is meaningful only in the
/// ordering test above.
#[test]
fn compute_pagerank_sparse_is_mass_preserving_and_finite() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranks =
        compute_pagerank_sparse(&graph.graph, PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS);

    assert_eq!(ranks.len(), graph.node_count());
    assert!(ranks.iter().all(|r| r.is_finite() && *r >= 0.0));
    let sum: f64 = ranks.iter().sum();
    assert!((sum - 1.0).abs() < 1e-9, "ranks must sum to ~1, got {sum}");
}

#[test]
fn compute_pagerank_sparse_handles_empty_graph() {
    let graph: DiGraph<RepoGraphNode, RepoGraphEdge> = DiGraph::new();
    let ranks = compute_pagerank_sparse(&graph, PAGE_RANK_DAMPING_FACTOR, 5);
    assert!(ranks.is_empty());
}

#[test]
fn page_rank_ordering_favors_referenced_symbols_and_files() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();

    let helper_symbol_rank = ranking
        .nodes
        .iter()
        .position(|node| {
            node.key == RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string())
        })
        .expect("helper symbol should be ranked");
    let app_symbol_rank = ranking
        .nodes
        .iter()
        .position(|node| {
            node.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string())
        })
        .expect("main symbol should be ranked");
    let helper_file_rank = ranking
        .nodes
        .iter()
        .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/helper.rs")))
        .expect("helper file should be ranked");
    let app_file_rank = ranking
        .nodes
        .iter()
        .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/app.rs")))
        .expect("app file should be ranked");

    // PR F4: positions are now governed by fused rank (RRF over
    // pagerank, total degree, entry-point distance), so we no
    // longer assert the legacy "helper outranks main" position
    // ordering — `main` is an entry point and the fusion now
    // promotes it. The classic `pagerank * structural_weight`
    // score is still surfaced on the node for callers that want
    // it, and we keep asserting that signal directly so the
    // PageRank pass itself doesn't silently regress.
    let helper_symbol_score = ranking.nodes[helper_symbol_rank].score;
    let app_symbol_score = ranking.nodes[app_symbol_rank].score;
    assert!(helper_symbol_score > app_symbol_score);

    let helper_file_score = ranking.nodes[helper_file_rank].score;
    let app_file_score = ranking.nodes[app_file_rank].score;
    assert!(helper_file_score > app_file_score);
}

/// PR F4: the entry-point function detected for the fixture
/// (`fn main` in `src/app.rs`) must come back from `rank()` with
/// `entry_point_distance == Some(0)` — distance is measured from
/// the entry-point set itself, BFS via Outgoing edges.
#[test]
fn entry_point_distance_zero_at_entry_point() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();
    let main_node = ranking
        .nodes
        .iter()
        .find(|n| n.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string()))
        .expect("main symbol should be in ranking");
    assert!(
        main_node.is_entry_point,
        "fixture's `fn main` should have been detected as an entry point",
    );
    assert_eq!(
        main_node.entry_point_distance,
        Some(0),
        "entry-point function should sit at distance 0",
    );
}

/// PR F4: build a tiny synthetic graph with two symbols at
/// identical PageRank — one is the entry point, the other is a
/// helper that lives off to the side. With RRF the entry-point
/// signal breaks the tie and the entry point ranks higher.
#[test]
fn rrf_fused_rank_promotes_entry_points_under_pagerank_tie() {
    // Hand-build the ranked node vector to control the inputs
    // exactly — using the SCIP fixture pulls in too much
    // structural variation to guarantee a strict pagerank tie.
    let entry_key = RepoNodeKey::Symbol("symbol:entry".to_string());
    let helper_key = RepoNodeKey::Symbol("symbol:helper".to_string());
    let mut nodes = vec![
        RankedRepoGraphNode {
            node_index: NodeIndex::new(0),
            key: entry_key.clone(),
            kind: RepoGraphNodeKind::Symbol,
            score: 0.5,
            page_rank: 0.5,
            structural_weight: 1.0,
            inbound_edge_weight: 1.0,
            outbound_edge_weight: 1.0,
            is_entry_point: true,
            entry_point_distance: Some(0),
            fused_rank: 0.0,
        },
        RankedRepoGraphNode {
            node_index: NodeIndex::new(1),
            key: helper_key.clone(),
            kind: RepoGraphNodeKind::Symbol,
            score: 0.5,
            page_rank: 0.5,
            structural_weight: 1.0,
            inbound_edge_weight: 1.0,
            outbound_edge_weight: 1.0,
            is_entry_point: false,
            entry_point_distance: None,
            fused_rank: 0.0,
        },
    ];
    apply_rrf_fused_rank(&mut nodes);
    nodes.sort_by(|l, r| r.fused_rank.total_cmp(&l.fused_rank));
    assert_eq!(
        nodes[0].key, entry_key,
        "entry point must outrank helper under RRF when pagerank/degree are tied",
    );
    assert_eq!(nodes[1].key, helper_key);
    assert!(
        nodes[0].fused_rank > nodes[1].fused_rank,
        "fused rank for entry point ({}) should exceed helper ({})",
        nodes[0].fused_rank,
        nodes[1].fused_rank,
    );
}

fn fixture_index() -> ParsedScipIndex {
    let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
    let helper_symbol = ScipSymbol {
        symbol: helper_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("helper".to_string()),
        signature: Some("fn helper()".to_string()),
        documentation: vec!["returns a value".to_string()],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let trait_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("HelperTrait".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let main_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("main".to_string()),
        signature: Some("fn main()".to_string()),
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
            target_symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
            kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/helper.rs"),
                definitions: vec![definition_occurrence(&helper_symbol_name)],
                references: vec![],
                occurrences: vec![definition_occurrence(&helper_symbol_name)],
                symbols: vec![helper_symbol],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/app.rs"),
                definitions: vec![definition_occurrence(&main_symbol.symbol)],
                references: vec![reference_occurrence(&helper_symbol_name)],
                occurrences: vec![
                    definition_occurrence(&main_symbol.symbol),
                    reference_occurrence(&helper_symbol_name),
                ],
                symbols: vec![main_symbol, trait_symbol],
            },
        ],
        external_symbols: vec![],
    }
}

fn definition_occurrence(symbol: &str) -> ScipOccurrence {
    ScipOccurrence {
        symbol: symbol.to_string(),
        range: ScipRange {
            start_line: 0,
            start_character: 0,
            end_line: 0,
            end_character: 6,
        },
        enclosing_range: None,
        roles: BTreeSet::from([ScipSymbolRole::Definition]),
        syntax_kind: None,
        override_documentation: vec![],
    }
}

fn reference_occurrence(symbol: &str) -> ScipOccurrence {
    ScipOccurrence {
        symbol: symbol.to_string(),
        range: ScipRange {
            start_line: 1,
            start_character: 4,
            end_line: 1,
            end_character: 10,
        },
        enclosing_range: None,
        roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
        syntax_kind: None,
        override_documentation: vec![],
    }
}

/// v8 end-to-end: simulate a rust-analyzer SCIP feed (no `ReadAccess`
/// or `WriteAccess` role bits on any occurrence) against a real
/// on-disk Rust file. The classifier should recover the read/write
/// distinction from AST context, producing `Reads` and `Writes`
/// edges where v7 would have emitted only `SymbolReference`.
///
/// This is the reference verification for the `build_with_source`
/// path — proves the fallback fires end-to-end and not just in the
/// classifier's unit tests.
#[test]
fn build_with_source_recovers_reads_and_writes_when_scip_roles_absent() {
    // A real Rust source file where `counter` is written on one line
    // and read on another. `value` is a definition site (not an
    // access). The line/column positions below match these
    // identifiers exactly.
    //
    // Layout (0-indexed):
    //   line 0: pub static mut COUNTER: i32 = 0;
    //   line 1:
    //   line 2: pub fn bump() {
    //   line 3:     unsafe { COUNTER = COUNTER + 1; }
    //                        ^^^^^^^   ^^^^^^^
    //                        col 13    col 23
    //   line 4: }
    let source = "pub static mut COUNTER: i32 = 0;\n\n\
                  pub fn bump() {\n    \
                  unsafe { COUNTER = COUNTER + 1; }\n}\n";
    let tempdir = tempfile::tempdir().expect("tempdir");
    let rel = PathBuf::from("src/counter.rs");
    let abs = tempdir.path().join(&rel);
    std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    std::fs::write(&abs, source).expect("write");

    let counter_symbol = "scip-rust pkg src/counter.rs `COUNTER`.".to_string();
    let counter_def_sym = ScipSymbol {
        symbol: counter_symbol.clone(),
        kind: Some(ScipSymbolKind::Variable),
        display_name: Some("COUNTER".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    // Definition occurrence at line 0 (the `static mut COUNTER`).
    let def = ScipOccurrence {
        symbol: counter_symbol.clone(),
        range: ScipRange {
            start_line: 0,
            start_character: 15,
            end_line: 0,
            end_character: 22,
        },
        enclosing_range: None,
        roles: BTreeSet::from([ScipSymbolRole::Definition]),
        syntax_kind: None,
        override_documentation: vec![],
    };
    // Reference occurrence at line 3 column 13 (`COUNTER = …` LHS) —
    // EMPTY role bits, mirroring rust-analyzer's SCIP output.
    let write_ref = ScipOccurrence {
        symbol: counter_symbol.clone(),
        range: ScipRange {
            start_line: 3,
            start_character: 13,
            end_line: 3,
            end_character: 20,
        },
        enclosing_range: None,
        roles: BTreeSet::new(),
        syntax_kind: None,
        override_documentation: vec![],
    };
    // Reference occurrence at line 3 column 23 (`= COUNTER + 1` RHS).
    let read_ref = ScipOccurrence {
        symbol: counter_symbol.clone(),
        range: ScipRange {
            start_line: 3,
            start_character: 23,
            end_line: 3,
            end_character: 30,
        },
        enclosing_range: None,
        roles: BTreeSet::new(),
        syntax_kind: None,
        override_documentation: vec![],
    };

    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: rel.clone(),
            definitions: vec![def.clone()],
            references: vec![write_ref.clone(), read_ref.clone()],
            occurrences: vec![def, write_ref, read_ref],
            symbols: vec![counter_def_sym],
        }],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build_with_source(&[index], Some(tempdir.path()));

    // Walk every edge with the COUNTER symbol as source and the
    // counter.rs file as target — there should be exactly one
    // `Writes` and one `Reads`, no `SymbolReference` fallbacks.
    let symbol_idx = graph
        .symbol_node(&counter_symbol)
        .expect("COUNTER symbol node should exist");
    let file_idx = graph
        .file_node(&rel)
        .expect("counter.rs file node should exist");
    let mut writes = 0usize;
    let mut reads = 0usize;
    let mut other = 0usize;
    for edge in graph
        .graph()
        .edges_directed(symbol_idx, petgraph::Direction::Outgoing)
    {
        if edge.target() != file_idx {
            continue;
        }
        match edge.weight().kind {
            RepoGraphEdgeKind::Writes => writes += 1,
            RepoGraphEdgeKind::Reads => reads += 1,
            RepoGraphEdgeKind::SymbolReference => other += 1,
            _ => {}
        }
    }
    assert_eq!(
        writes, 1,
        "tree-sitter classifier should have recovered exactly one Writes edge \
         (counter.rs `COUNTER = …` LHS)"
    );
    assert_eq!(
        reads, 1,
        "tree-sitter classifier should have recovered exactly one Reads edge \
         (counter.rs `… = COUNTER + 1` RHS)"
    );
    assert_eq!(
        other, 0,
        "no SymbolReference fallback edges should remain when the classifier \
         can resolve the AST context"
    );
}

#[test]
fn artifact_round_trip_preserves_graph_structure() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let original_node_count = graph.node_count();
    let original_edge_count = graph.edge_count();

    let artifact = graph.to_artifact();
    assert_eq!(artifact.nodes.len(), original_node_count);
    assert_eq!(artifact.edges.len(), original_edge_count);

    let restored = RepoDependencyGraph::from_artifact(&artifact);
    assert_eq!(restored.node_count(), original_node_count);
    assert_eq!(restored.edge_count(), original_edge_count);

    // Verify file and symbol lookups still work after round-trip.
    assert!(restored.file_node("src/app.rs").is_some());
    assert!(restored.file_node("src/helper.rs").is_some());
    assert!(
        restored
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .is_some()
    );
    assert!(
        restored
            .symbol_node("scip-rust pkg src/app.rs `main`().")
            .is_some()
    );

    // Verify ranking still produces valid results.
    let ranking = restored.rank();
    assert!(!ranking.nodes.is_empty());
}

#[test]
fn build_stamps_workspace_from_parsed_index() {
    let mut index = fixture_index();
    index.workspace_slug = "api".to_string();
    let graph = RepoDependencyGraph::build(&[index]);

    let app_file = graph.file_node("src/app.rs").expect("app file");
    assert_eq!(graph.node(app_file).workspace.as_deref(), Some("api"));

    let helper_symbol = graph
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper symbol");
    assert_eq!(graph.node(helper_symbol).workspace.as_deref(), Some("api"));

    for node in graph
        .graph()
        .node_weights()
        .filter(|node| node.kind == RepoGraphNodeKind::Process)
    {
        assert_eq!(node.workspace, None);
    }
}

#[test]
fn artifact_json_round_trip_preserves_graph() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let json = graph.serialize_artifact().expect("serialize");
    let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize");

    assert_eq!(restored.node_count(), graph.node_count());
    assert_eq!(restored.edge_count(), graph.edge_count());

    // Verify node metadata survived serialization.
    let helper_idx = restored
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper symbol");
    let helper_node = restored.node(helper_idx);
    assert_eq!(helper_node.symbol_kind, Some(ScipSymbolKind::Function));
    assert_eq!(helper_node.display_name, "helper");
    assert_eq!(
        helper_node.file_path.as_deref(),
        Some(Path::new("src/helper.rs"))
    );

    // Verify edge metadata survived.
    let app_idx = restored.file_node("src/app.rs").expect("app file");
    let has_contains_def = restored
        .graph()
        .edges(app_idx)
        .any(|e| e.weight().kind == RepoGraphEdgeKind::ContainsDefinition);
    assert!(
        has_contains_def,
        "expected ContainsDefinition edge from app file"
    );
}

#[test]
fn empty_artifact_round_trip() {
    let empty = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![],
        edges: vec![],
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: vec![],
    };
    let json = serde_json::to_string(&empty).expect("serialize empty");
    let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize empty");
    assert_eq!(restored.node_count(), 0);
    assert_eq!(restored.edge_count(), 0);
}

// ── PR A2: edge confidence + reason ───────────────────────────────────

/// Every edge kind emitted by the builder gets a confidence value within
/// `(0, 1]`. Sweeping the fixture is the cheapest way to assert "no kind
/// silently slipped through with the default 0.0".
#[test]
fn every_edge_kind_carries_a_confidence_value_pr_a2() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    assert!(graph.edge_count() > 0, "fixture must produce edges");

    let mut seen_kinds: BTreeSet<RepoGraphEdgeKind> = BTreeSet::new();
    for edge_ref in graph.graph().edge_references() {
        let edge = edge_ref.weight();
        seen_kinds.insert(edge.kind);
        assert!(
            edge.confidence > 0.0 && edge.confidence <= 1.0,
            "edge {:?} has out-of-range confidence {}",
            edge.kind,
            edge.confidence
        );
        // Confidence must equal the floor for the kind, optionally
        // dropped by exactly the local-prefix penalty when the reason
        // says so. PR F1: `EntryPointOf` edges set their own
        // confidence (per-detector, 0.6 – 0.95) so the floor check
        // doesn't apply — we just bound them in (0, 1]. PR F2:
        // `StepInProcess` edges always carry `reason="process-step"`
        // and stick to the floor; we whitelist that reason.
        if edge.kind == RepoGraphEdgeKind::EntryPointOf {
            continue;
        }
        let floor = edge_confidence_floor(edge.kind);
        match edge.reason.as_deref() {
            None => assert_eq!(edge.confidence, floor),
            Some("local-prefix") => {
                let expected = (floor - EDGE_CONFIDENCE_LOCAL_PENALTY).clamp(0.0, 1.0);
                assert!(
                    (edge.confidence - expected).abs() < 1e-9,
                    "local-prefix edge confidence {} != expected {} for kind {:?}",
                    edge.confidence,
                    expected,
                    edge.kind
                );
            }
            Some("process-step") => {
                assert_eq!(edge.kind, RepoGraphEdgeKind::StepInProcess);
                assert!((edge.confidence - floor).abs() < 1e-9);
            }
            Some(other) => panic!("unexpected reason {other:?} on edge {:?}", edge.kind),
        }
    }
    // The fixture exercises Contains/DeclaredIn/FileRef/Reads (post-A3
    // the helper-call read-access reference is classified as `Reads`
    // rather than the generic `SymbolReference`) and an Implementation
    // relationship — every code path that can mint an edge today.
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::ContainsDefinition));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::DeclaredInFile));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::FileReference));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::Reads));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::Implements));
}

/// Bincode round-trip preserves `confidence` and `reason` on every edge.
/// This is the core "artifact v1" guarantee — old blobs without these
/// fields will fail to deserialize and trigger a warm rebuild.
#[test]
fn bincode_round_trip_preserves_edge_confidence_and_reason_pr_a2() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    assert_eq!(artifact.version, REPO_GRAPH_ARTIFACT_VERSION);

    // Snapshot original (kind, confidence, reason) tuples by sorting on
    // (kind, confidence, reason) — edge_count is small so a Vec<_> is
    // fine.
    let mut original: Vec<(RepoGraphEdgeKind, f64, Option<String>)> = artifact
        .edges
        .iter()
        .map(|e| (e.kind, e.confidence, e.reason.clone()))
        .collect();
    original.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
    });

    let encoded = bincode::serialize(&artifact).expect("bincode serialize");
    let decoded: RepoGraphArtifact = bincode::deserialize(&encoded).expect("bincode deserialize");
    assert_eq!(decoded.version, REPO_GRAPH_ARTIFACT_VERSION);

    let mut round_tripped: Vec<(RepoGraphEdgeKind, f64, Option<String>)> = decoded
        .edges
        .iter()
        .map(|e| (e.kind, e.confidence, e.reason.clone()))
        .collect();
    round_tripped.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
    });

    assert_eq!(round_tripped, original);

    // And that survives the `from_artifact` rebuild path that powers
    // `load_canonical_graph`.
    let restored = RepoDependencyGraph::from_artifact(&decoded);
    for edge_ref in restored.graph().edge_references() {
        let edge = edge_ref.weight();
        assert!(edge.confidence > 0.0 && edge.confidence <= 1.0);
    }
}

#[test]
fn bincode_v10_artifact_without_workspace_deserializes_with_none() {
    #[derive(Serialize)]
    struct V10RepoGraphNodeWithoutWorkspace {
        id: RepoNodeKey,
        kind: RepoGraphNodeKind,
        display_name: String,
        language: Option<String>,
        file_path: Option<PathBuf>,
        symbol: Option<String>,
        symbol_kind: Option<ScipSymbolKind>,
        is_external: bool,
        visibility: Option<crate::scip_parser::ScipVisibility>,
        signature: Option<String>,
        documentation: Vec<String>,
        signature_parts: Option<crate::scip_parser::ScipSignatureParts>,
        is_test: bool,
        complexity: Option<ComplexityMetrics>,
    }

    #[derive(Serialize)]
    struct V10RepoGraphArtifactWithoutWorkspace {
        version: u32,
        nodes: Vec<V10RepoGraphNodeWithoutWorkspace>,
        edges: Vec<RepoGraphArtifactEdge>,
        symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
        communities: Vec<crate::communities::Community>,
        processes: Vec<RepoGraphArtifactProcess>,
    }

    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    let old_nodes = artifact
        .nodes
        .iter()
        .map(|node| V10RepoGraphNodeWithoutWorkspace {
            id: node.id.clone(),
            kind: node.kind,
            display_name: node.display_name.clone(),
            language: node.language.clone(),
            file_path: node.file_path.clone(),
            symbol: node.symbol.clone(),
            symbol_kind: node.symbol_kind.clone(),
            is_external: node.is_external,
            visibility: node.visibility,
            signature: node.signature.clone(),
            documentation: node.documentation.clone(),
            signature_parts: node.signature_parts.clone(),
            is_test: node.is_test,
            complexity: node.complexity,
        })
        .collect();
    let old_artifact = V10RepoGraphArtifactWithoutWorkspace {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: old_nodes,
        edges: artifact.edges.clone(),
        symbol_ranges: artifact.symbol_ranges.clone(),
        communities: artifact.communities.clone(),
        processes: artifact.processes.clone(),
    };

    let encoded = bincode::serialize(&old_artifact).expect("serialize old v10 bincode");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize old v10 bincode through compatibility path");

    assert_eq!(decoded.version, REPO_GRAPH_ARTIFACT_VERSION);
    assert!(!decoded.nodes.is_empty());
    assert!(decoded.nodes.iter().all(|node| node.workspace.is_none()));

    let restored = RepoDependencyGraph::from_artifact(&decoded);
    let app_file = restored.file_node("src/app.rs").expect("app file");
    assert_eq!(restored.node(app_file).workspace, None);
}

/// A `local`-prefixed symbol triggers `reason="local-prefix"` and a
/// confidence drop of exactly `EDGE_CONFIDENCE_LOCAL_PENALTY` from the
/// kind's floor. This is the only signal the visibility heuristic
/// surfaces today.
#[test]
fn local_prefix_symbol_triggers_local_prefix_reason_pr_a2() {
    // Build a tiny synthetic index where one of the symbols is local.
    let local_symbol_name = "local 42".to_string();
    let local_sym = ScipSymbol {
        symbol: local_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Variable),
        display_name: Some("local_var".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Private),
        signature_parts: None,
    };
    let pub_sym = ScipSymbol {
        symbol: "scip-rust pkg src/main.rs `caller`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("caller".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: "scip-rust pkg src/main.rs `caller`().".to_string(),
            target_symbol: local_symbol_name.clone(),
            kinds: BTreeSet::from([ScipRelationshipKind::Reference]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("src/main.rs"),
            definitions: vec![definition_occurrence(&pub_sym.symbol)],
            references: vec![],
            occurrences: vec![definition_occurrence(&pub_sym.symbol)],
            symbols: vec![pub_sym, local_sym],
        }],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build(&[index]);
    let mut saw_local_prefix = false;
    for edge_ref in graph.graph().edge_references() {
        let edge = edge_ref.weight();
        if edge.reason.as_deref() == Some("local-prefix") {
            saw_local_prefix = true;
            let floor = edge_confidence_floor(edge.kind);
            let expected = (floor - EDGE_CONFIDENCE_LOCAL_PENALTY).clamp(0.0, 1.0);
            assert!(
                (edge.confidence - expected).abs() < 1e-9,
                "expected confidence {expected}, got {} for kind {:?}",
                edge.confidence,
                edge.kind
            );
        }
    }
    assert!(
        saw_local_prefix,
        "expected at least one edge involving the `local 42` symbol to be flagged"
    );
}

// ── PR A3: SymbolReference read/write split ───────────────────────────

/// Build a fixture project with a struct field that is read in one file
/// and written in another. Assert the role-aware split classifies the
/// edges as `Reads` / `Writes`. This is the core behaviour callers rely
/// on for `code_graph neighbors --kind_filter=writes` (PR A3 acceptance).
#[test]
fn read_write_split_classifies_field_accesses_pr_a3() {
    // Field `Counter#value`. Lives in src/counter.rs; is written from
    // src/writer.rs (mutator) and read from src/reader.rs (observer).
    let field_symbol = "scip-rust pkg src/counter.rs `Counter`#`value`.".to_string();
    let counter_struct = ScipSymbol {
        symbol: "scip-rust pkg src/counter.rs `Counter`#".to_string(),
        kind: Some(ScipSymbolKind::Struct),
        display_name: Some("Counter".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let value_field = ScipSymbol {
        symbol: field_symbol.clone(),
        kind: Some(ScipSymbolKind::Field),
        display_name: Some("value".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    let writer_sym = ScipSymbol {
        symbol: "scip-rust pkg src/writer.rs `bump`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("bump".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let reader_sym = ScipSymbol {
        symbol: "scip-rust pkg src/reader.rs `peek`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("peek".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    // Helper: build an occurrence with explicit roles, since the
    // existing `definition_occurrence` / `reference_occurrence` helpers
    // hardcode their role sets.
    fn role_occurrence(symbol: &str, roles: BTreeSet<ScipSymbolRole>) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles,
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/counter.rs"),
                definitions: vec![
                    definition_occurrence(&counter_struct.symbol),
                    definition_occurrence(&field_symbol),
                ],
                references: vec![],
                occurrences: vec![
                    definition_occurrence(&counter_struct.symbol),
                    definition_occurrence(&field_symbol),
                ],
                symbols: vec![counter_struct, value_field],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/writer.rs"),
                definitions: vec![definition_occurrence(&writer_sym.symbol)],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::WriteAccess]),
                )],
                occurrences: vec![
                    definition_occurrence(&writer_sym.symbol),
                    role_occurrence(&field_symbol, BTreeSet::from([ScipSymbolRole::WriteAccess])),
                ],
                symbols: vec![writer_sym],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/reader.rs"),
                definitions: vec![definition_occurrence(&reader_sym.symbol)],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::ReadAccess]),
                )],
                occurrences: vec![
                    definition_occurrence(&reader_sym.symbol),
                    role_occurrence(&field_symbol, BTreeSet::from([ScipSymbolRole::ReadAccess])),
                ],
                symbols: vec![reader_sym],
            },
        ],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build(&[index]);

    // The role-aware classification fires on the symbol→target_file
    // edge minted in `add_reference`. Locate the field-defining file
    // and the writer/reader source files.
    let counter_file = graph
        .file_node("src/counter.rs")
        .expect("counter file node");
    let writer_file = graph.file_node("src/writer.rs").expect("writer file node");
    let reader_file = graph.file_node("src/reader.rs").expect("reader file node");
    let field_node = graph.symbol_node(&field_symbol).expect("field node");

    // From the WRITE site, the field's symbol→counter_file edge should
    // be tagged `Writes`. The field has *both* a writer site and a
    // reader site, so to attribute the right kind to the right source
    // we sweep edges *out of the field node* — there is one
    // SymbolReference-class edge per occurrence kind into
    // `src/counter.rs`. We expect exactly one `Writes` and one `Reads`
    // edge from the field node into the counter file.
    let mut writes_count = 0;
    let mut reads_count = 0;
    let mut bare_ref_count = 0;
    for edge in graph
        .graph()
        .edges_directed(field_node, petgraph::Direction::Outgoing)
    {
        if edge.target() != counter_file {
            continue;
        }
        match edge.weight().kind {
            RepoGraphEdgeKind::Writes => writes_count += 1,
            RepoGraphEdgeKind::Reads => reads_count += 1,
            RepoGraphEdgeKind::SymbolReference => bare_ref_count += 1,
            _ => {}
        }
    }
    assert_eq!(
        writes_count, 1,
        "expected exactly one Writes edge from field to counter file"
    );
    assert_eq!(
        reads_count, 1,
        "expected exactly one Reads edge from field to counter file"
    );
    assert_eq!(
        bare_ref_count, 0,
        "no bare SymbolReference edge should remain after the split when SCIP roles are populated"
    );

    // And the confidence floors land on the values pinned in this PR.
    for edge in graph.graph().edge_references() {
        let edge = edge.weight();
        match edge.kind {
            RepoGraphEdgeKind::Writes => {
                assert!(
                    (edge.confidence - EDGE_CONFIDENCE_WRITES).abs() < 1e-9,
                    "Writes edge confidence {} != floor {}",
                    edge.confidence,
                    EDGE_CONFIDENCE_WRITES
                );
            }
            RepoGraphEdgeKind::Reads => {
                assert!(
                    (edge.confidence - EDGE_CONFIDENCE_READS).abs() < 1e-9,
                    "Reads edge confidence {} != floor {}",
                    edge.confidence,
                    EDGE_CONFIDENCE_READS
                );
            }
            _ => {}
        }
    }

    // The unused vars silence "fields never read" without dropping
    // the assertions.
    let _ = (writer_file, reader_file);
}

/// `kind_filter=writes` on the field's neighbors picks out the file
/// that *writes* it; `reads` picks out the *reader*. This is the
/// acceptance criterion in the PR A3 plan: "fixture project with a
/// struct field; assert `kind_filter=writes` returns only writers,
/// `reads` only readers".
///
/// The `neighbors` op itself lives in the bridge; here we exercise the
/// underlying graph the bridge filters over.
#[test]
fn neighbors_kind_filter_writes_returns_only_writers_pr_a3() {
    // Reuse the multi-file fixture set up by the previous test by
    // inlining a smaller variant focused on just the field node.
    let field_symbol = "scip-rust pkg src/counter.rs `f`#`v`.".to_string();
    let value_field = ScipSymbol {
        symbol: field_symbol.clone(),
        kind: Some(ScipSymbolKind::Field),
        display_name: Some("v".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    fn role_occurrence(symbol: &str, roles: BTreeSet<ScipSymbolRole>) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles,
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/counter.rs"),
                definitions: vec![definition_occurrence(&field_symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&field_symbol)],
                symbols: vec![value_field],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/writer.rs"),
                definitions: vec![],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::WriteAccess]),
                )],
                occurrences: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::WriteAccess]),
                )],
                symbols: vec![],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/reader.rs"),
                definitions: vec![],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::ReadAccess]),
                )],
                occurrences: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::ReadAccess]),
                )],
                symbols: vec![],
            },
        ],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build(&[index]);
    let counter_file = graph
        .file_node("src/counter.rs")
        .expect("counter file node");

    // Mirror the bridge-side filter: walk outgoing edges from the
    // field node into `src/counter.rs` and partition by edge kind.
    let field_node = graph.symbol_node(&field_symbol).expect("field node");
    let writes: Vec<_> = graph
        .graph()
        .edges_directed(field_node, petgraph::Direction::Outgoing)
        .filter(|e| e.target() == counter_file && e.weight().kind == RepoGraphEdgeKind::Writes)
        .collect();
    let reads: Vec<_> = graph
        .graph()
        .edges_directed(field_node, petgraph::Direction::Outgoing)
        .filter(|e| e.target() == counter_file && e.weight().kind == RepoGraphEdgeKind::Reads)
        .collect();

    assert_eq!(writes.len(), 1, "expected one Writes edge");
    assert_eq!(reads.len(), 1, "expected one Reads edge");

    // Confidence floors propagate from the table in this module.
    assert!(
        (writes[0].weight().confidence - EDGE_CONFIDENCE_WRITES).abs() < 1e-9,
        "Writes confidence floor mismatch"
    );
    assert!(
        (reads[0].weight().confidence - EDGE_CONFIDENCE_READS).abs() < 1e-9,
        "Reads confidence floor mismatch"
    );
}

// ---- patch_changed_files tests ----

/// Build a modified index where src/app.rs has a new symbol and a removed
/// reference, then patch the original graph and verify the result reflects
/// the changes.
#[test]
fn patch_changed_files_updates_graph_for_modified_file() {
    let original = RepoDependencyGraph::build(&[fixture_index()]);

    // The original graph has src/helper.rs and src/app.rs.
    assert!(original.file_node("src/app.rs").is_some());
    assert!(original.file_node("src/helper.rs").is_some());
    assert!(
        original
            .symbol_node("scip-rust pkg src/app.rs `main`().")
            .is_some()
    );

    // Build a replacement for src/app.rs that has a new symbol "run"
    // instead of "main" and no reference to helper.
    let run_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/app.rs `run`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("run".to_string()),
        signature: Some("fn run()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let new_index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("src/app.rs"),
            definitions: vec![definition_occurrence(&run_symbol.symbol)],
            references: vec![],
            occurrences: vec![definition_occurrence(&run_symbol.symbol)],
            symbols: vec![run_symbol],
        }],
        external_symbols: vec![],
    };

    let changed = BTreeSet::from([PathBuf::from("src/app.rs")]);
    let patched = original.patch_changed_files(&changed, &[new_index]);

    // The old "main" symbol should be gone; the new "run" symbol should exist.
    assert!(
        patched
            .symbol_node("scip-rust pkg src/app.rs `main`().")
            .is_none(),
        "old main symbol should be removed after patch"
    );
    assert!(
        patched
            .symbol_node("scip-rust pkg src/app.rs `run`().")
            .is_some(),
        "new run symbol should be present after patch"
    );

    // src/helper.rs and its symbol should be untouched.
    assert!(patched.file_node("src/helper.rs").is_some());
    assert!(
        patched
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .is_some()
    );

    // src/app.rs file node should still exist (re-added by the new index).
    assert!(patched.file_node("src/app.rs").is_some());

    // Ranking should still work and produce valid output.
    let patched_ranking = patched.rank();
    assert!(!patched_ranking.nodes.is_empty());

    // The helper symbol should still rank high (it was not changed).
    let helper_rank = patched_ranking
        .nodes
        .iter()
        .position(|n| {
            n.key == RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string())
        })
        .expect("helper should be ranked");
    assert!(helper_rank < patched_ranking.nodes.len());
}

/// Patching with an empty changed-file set produces the same graph.
#[test]
fn patch_with_no_changed_files_preserves_graph() {
    let original = RepoDependencyGraph::build(&[fixture_index()]);
    let changed: BTreeSet<PathBuf> = BTreeSet::new();
    let patched = original.patch_changed_files(&changed, &[]);
    assert_eq!(patched.node_count(), original.node_count());
    assert_eq!(patched.edge_count(), original.edge_count());
}

/// Patching a file that does not exist in the graph is a no-op for removal
/// and just adds new data.
#[test]
fn patch_nonexistent_file_adds_new_data() {
    let original = RepoDependencyGraph::build(&[fixture_index()]);
    let original_node_count = original.node_count();

    let new_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/new.rs `new_fn`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("new_fn".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let new_index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("src/new.rs"),
            definitions: vec![definition_occurrence(&new_symbol.symbol)],
            references: vec![],
            occurrences: vec![definition_occurrence(&new_symbol.symbol)],
            symbols: vec![new_symbol],
        }],
        external_symbols: vec![],
    };

    let changed = BTreeSet::from([PathBuf::from("src/new.rs")]);
    let patched = original.patch_changed_files(&changed, &[new_index]);

    // New file and symbol added.
    assert!(patched.file_node("src/new.rs").is_some());
    assert!(
        patched
            .symbol_node("scip-rust pkg src/new.rs `new_fn`().")
            .is_some()
    );
    // Original nodes preserved.
    assert!(patched.node_count() > original_node_count);
    assert!(patched.file_node("src/app.rs").is_some());
    assert!(patched.file_node("src/helper.rs").is_some());
}

// ── Chunk B: search / cycles / orphans / path tests ─────────────────────

#[test]
fn search_by_name_finds_substring_and_ranks_exact_first() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let hits = graph.search_by_name("helper", None, 10);
    assert!(!hits.is_empty(), "expected at least one hit for 'helper'");
    // The exact-name hit (display_name = "helper") should be first.
    let first = &graph.node(hits[0].node_index).display_name;
    assert_eq!(first.to_lowercase(), "helper");
}

#[test]
fn search_by_name_respects_kind_filter() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let hits = graph.search_by_name("helper", Some(RepoGraphNodeKind::Symbol), 10);
    for hit in &hits {
        assert_eq!(graph.node(hit.node_index).kind, RepoGraphNodeKind::Symbol);
    }
}

// F6: query-planner search path. The OFF path must be identical to the
// baseline `search_by_name`; the ON path unions across sub-queries.
#[test]
fn search_by_name_planned_off_matches_baseline() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let baseline = graph.search_by_name("helper", None, 10);

    // No planner injected ⇒ pass-through regardless of the env flag.
    let planned_none = graph.search_by_name_planned("helper", None, 10, None);
    assert_eq!(
        baseline.len(),
        planned_none.len(),
        "OFF path (no planner) must match baseline length"
    );
    for (b, p) in baseline.iter().zip(planned_none.iter()) {
        assert_eq!(b.node_index, p.node_index);
        assert_eq!(b.score, p.score);
    }

    // Even with a planner supplied, the gating flag is OFF by default
    // (CI runs without DJINN_CODE_GRAPH_QUERY_PLANNER set), so the
    // planner is never invoked and results still equal the baseline.
    if std::env::var(crate::query_planner::QUERY_PLANNER_FLAG).is_err() {
        let planner = crate::query_planner::StaticPlanner::new(vec!["app".into(), "main".into()]);
        let planned_with = graph.search_by_name_planned("helper", None, 10, Some(&planner));
        assert_eq!(baseline.len(), planned_with.len());
        for (b, p) in baseline.iter().zip(planned_with.iter()) {
            assert_eq!(b.node_index, p.node_index);
        }
    }
}

#[test]
fn search_by_name_planned_on_unions_subqueries() {
    use crate::query_planner::{StaticPlanner, union_dedup_hits};
    let graph = RepoDependencyGraph::build(&[fixture_index()]);

    // Simulate the ON path deterministically (without touching shared
    // process env): plan -> per-sub-query search -> union+dedup, and
    // assert the union is a superset of any single sub-query's hits.
    let planner = StaticPlanner::new(vec!["app".into()]);
    let plan = crate::query_planner::plan_query(&planner, "helper");
    assert!(plan.len() >= 2, "expected the planner to expand the query");

    let per_query: Vec<_> = plan
        .iter()
        .map(|q| graph.search_by_name(q, None, 10))
        .collect();
    let helper_only = graph.search_by_name("helper", None, 10);
    let app_only = graph.search_by_name("app", None, 10);
    let union = union_dedup_hits(per_query, 50);

    // Union contains every node from each individual sub-query search.
    for h in helper_only.iter().chain(app_only.iter()) {
        assert!(
            union.iter().any(|u| u.node_index == h.node_index),
            "union must include node {:?} from a sub-query",
            h.node_index
        );
    }
    // And no duplicate node indices survive the dedup.
    let mut seen = std::collections::HashSet::new();
    for u in &union {
        assert!(seen.insert(u.node_index), "duplicate node in union");
    }
}

#[test]
fn cycles_finds_symbol_cycle_via_relationships() {
    // Two mutually-referencing symbols via SCIP relationships create a
    // symbol-level cycle that tarjan_scc must report.
    let a_sym = ScipSymbol {
        symbol: "scip-rust pkg src/a.rs `a_fn`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("a_fn".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: "scip-rust pkg src/a.rs `a_fn`().".to_string(),
            target_symbol: "scip-rust pkg src/b.rs `b_fn`().".to_string(),
            kinds: BTreeSet::from([ScipRelationshipKind::Reference]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let b_sym = ScipSymbol {
        symbol: "scip-rust pkg src/b.rs `b_fn`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("b_fn".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: "scip-rust pkg src/b.rs `b_fn`().".to_string(),
            target_symbol: "scip-rust pkg src/a.rs `a_fn`().".to_string(),
            kinds: BTreeSet::from([ScipRelationshipKind::Reference]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/a.rs"),
                definitions: vec![definition_occurrence(&a_sym.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&a_sym.symbol)],
                symbols: vec![a_sym.clone()],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/b.rs"),
                definitions: vec![definition_occurrence(&b_sym.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&b_sym.symbol)],
                symbols: vec![b_sym.clone()],
            },
        ],
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[index]);
    let sccs = graph.strongly_connected_components(Some(RepoGraphNodeKind::Symbol), 2);
    let has_two_symbol_cycle = sccs.iter().any(|component| {
        component.len() >= 2
            && component
                .iter()
                .all(|n| graph.node(*n).kind == RepoGraphNodeKind::Symbol)
    });
    assert!(
        has_two_symbol_cycle,
        "expected a symbol-level cycle of size >= 2; got SCCs: {sccs:?}"
    );
}

#[test]
fn orphans_filters_by_visibility() {
    let public_unused = ScipSymbol {
        symbol: "scip-rust pkg src/lib.rs `PublicUnused`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("PublicUnused".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let private_unused = ScipSymbol {
        symbol: "local 1".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("private_unused".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Private),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("src/lib.rs"),
            definitions: vec![
                definition_occurrence(&public_unused.symbol),
                definition_occurrence(&private_unused.symbol),
            ],
            references: vec![],
            occurrences: vec![
                definition_occurrence(&public_unused.symbol),
                definition_occurrence(&private_unused.symbol),
            ],
            symbols: vec![public_unused.clone(), private_unused.clone()],
        }],
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[index]);

    let public_orphans = graph.orphans(
        Some(RepoGraphNodeKind::Symbol),
        Some(crate::scip_parser::ScipVisibility::Public),
        100,
    );
    let public_names: Vec<String> = public_orphans
        .iter()
        .map(|idx| graph.node(*idx).display_name.clone())
        .collect();
    assert!(public_names.iter().any(|n| n == "PublicUnused"));
    assert!(!public_names.iter().any(|n| n == "private_unused"));

    let private_orphans = graph.orphans(
        Some(RepoGraphNodeKind::Symbol),
        Some(crate::scip_parser::ScipVisibility::Private),
        100,
    );
    let private_names: Vec<String> = private_orphans
        .iter()
        .map(|idx| graph.node(*idx).display_name.clone())
        .collect();
    assert!(private_names.iter().any(|n| n == "private_unused"));
    assert!(!private_names.iter().any(|n| n == "PublicUnused"));
}

#[test]
fn shortest_path_finds_route_between_two_nodes() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let from = graph.file_node("src/app.rs").expect("app file");
    let to = graph
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper symbol");
    let path = graph
        .shortest_path(from, to, None)
        .expect("there should be a path from app to helper");
    assert!(path.len() >= 2);
    assert_eq!(path[0], from);
    assert_eq!(*path.last().unwrap(), to);
}

// ── symbols_enclosing tests ─────────────────────────────────────────────

/// Build a SCIP occurrence for a definition with an explicit enclosing
/// range (0-indexed, half-open-like on the wire; `symbols_enclosing`
/// normalizes to 1-indexed inclusive).
fn definition_with_enclosing(
    symbol: &str,
    enclosing_start: i32,
    enclosing_end: i32,
) -> ScipOccurrence {
    ScipOccurrence {
        symbol: symbol.to_string(),
        range: ScipRange {
            start_line: enclosing_start,
            start_character: 0,
            end_line: enclosing_start,
            end_character: 6,
        },
        enclosing_range: Some(ScipRange {
            start_line: enclosing_start,
            start_character: 0,
            end_line: enclosing_end,
            end_character: 0,
        }),
        roles: BTreeSet::from([ScipSymbolRole::Definition]),
        syntax_kind: None,
        override_documentation: vec![],
    }
}

/// Fixture with nested symbols in one file and a separate sibling file:
///
/// `src/lib.rs`:
/// - `outer` module, lines 1..20 (0-indexed: 0..=19)
/// - `Inner` struct, lines 5..12 (nested in outer)
/// - `inner_method` method, lines 7..10 (nested in Inner)
/// - `sibling_fn` function, lines 25..30 (sibling of outer)
///
/// `src/other.rs`:
/// - `other_fn` function, lines 1..5
fn nested_ranges_fixture() -> ParsedScipIndex {
    let outer_sym = ScipSymbol {
        symbol: "scip-rust pkg src/lib.rs `outer`/".to_string(),
        kind: Some(ScipSymbolKind::Namespace),
        display_name: Some("outer".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let inner_sym = ScipSymbol {
        symbol: "scip-rust pkg src/lib.rs `outer`/`Inner`#".to_string(),
        kind: Some(ScipSymbolKind::Struct),
        display_name: Some("Inner".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let method_sym = ScipSymbol {
        symbol: "scip-rust pkg src/lib.rs `outer`/`Inner`#`inner_method`().".to_string(),
        kind: Some(ScipSymbolKind::Method),
        display_name: Some("inner_method".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let sibling_sym = ScipSymbol {
        symbol: "scip-rust pkg src/lib.rs `sibling_fn`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("sibling_fn".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let other_sym = ScipSymbol {
        symbol: "scip-rust pkg src/other.rs `other_fn`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("other_fn".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/lib.rs"),
                // 0-indexed on the wire; the normalization adds 1 so the
                // resulting 1-indexed inclusive ranges are:
                //   outer:        1..=20
                //   Inner:        6..=12
                //   inner_method: 8..=10
                //   sibling_fn:  26..=30
                definitions: vec![
                    definition_with_enclosing(&outer_sym.symbol, 0, 19),
                    definition_with_enclosing(&inner_sym.symbol, 5, 11),
                    definition_with_enclosing(&method_sym.symbol, 7, 9),
                    definition_with_enclosing(&sibling_sym.symbol, 25, 29),
                ],
                references: vec![],
                occurrences: vec![],
                symbols: vec![
                    outer_sym.clone(),
                    inner_sym.clone(),
                    method_sym.clone(),
                    sibling_sym.clone(),
                ],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/other.rs"),
                // 1..=5 after normalization.
                definitions: vec![definition_with_enclosing(&other_sym.symbol, 0, 4)],
                references: vec![],
                occurrences: vec![],
                symbols: vec![other_sym.clone()],
            },
        ],
        external_symbols: vec![],
    }
}

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
