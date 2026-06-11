use super::*;

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

#[test]
fn search_by_name_none_zero_route_tool_nodes_keeps_fixture_order() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    assert!(graph.graph().node_weights().all(|node| {
        node.kind != RepoGraphNodeKind::Route && node.kind != RepoGraphNodeKind::Tool
    }));

    let hits = graph.search_by_name("helper", None, 10);
    let keys: Vec<_> = hits
        .iter()
        .map(|hit| graph.node(hit.node_index).key())
        .collect();

    assert_eq!(
        keys,
        vec![
            RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string()),
            RepoNodeKey::File(PathBuf::from("src/helper.rs")),
            RepoNodeKey::Symbol("scip-rust pkg src/types.rs `HelperTrait`#".to_string()),
        ],
        "adding Route/Tool model variants must not perturb the zero-new-kind fixture search contract"
    );
}

#[test]
fn default_search_and_query_subgraph_skip_route_tool_but_explicit_route_filter_finds_route() {
    let mut graph = RepoDependencyGraph::build(&[fixture_index()]);
    let handler_symbol = "scip-rust pkg src/helper.rs `helper`().";
    let caller_symbol = "scip-rust pkg src/app.rs `main`().";
    let handler = graph.symbol_node(handler_symbol).expect("handler symbol");
    let caller = graph.symbol_node(caller_symbol).expect("caller symbol");
    let route = graph.ensure_route_node(
        "GET /api/helper (axum)",
        "helper",
        Some("rust"),
        Some("root"),
        None,
        Some("axum"),
        Some(handler_symbol),
    );
    let tool = graph.ensure_tool_node("helper.run", "helper", Some("rust"), Some("root"), None);
    graph.add_handles_route_edge(route, handler, "axum-route-attr", Some(0.95));
    graph.add_fetches_edge(caller, route, "ts-fetch-literal", Some(0.75));

    let default_hits = graph.search_by_name("helper", None, 10);
    assert!(default_hits.iter().all(|hit| {
        let kind = graph.node(hit.node_index).kind;
        kind != RepoGraphNodeKind::Route && kind != RepoGraphNodeKind::Tool
    }));

    let route_hits = graph.search_by_name("helper", Some(RepoGraphNodeKind::Route), 10);
    assert_eq!(route_hits.len(), 1);
    assert_eq!(route_hits[0].node_index, route);

    let default_subgraph = graph.query_subgraph(
        crate::query_subgraph::QuerySubgraphParams {
            query: "helper".to_string(),
            workspace: None,
            context_filter: None,
            file_filter: None,
            kind_filter: None,
            edge_filter: Vec::new(),
            token_budget: None,
            max_depth: Some(2),
            max_seeds: Some(4),
            min_hub_degree: Some(100),
        },
        None,
    );
    assert!(default_subgraph.nodes.iter().all(|node| {
        node.kind != RepoGraphNodeKind::Route && node.kind != RepoGraphNodeKind::Tool
    }));

    let route_subgraph = graph.query_subgraph(
        crate::query_subgraph::QuerySubgraphParams {
            query: "helper".to_string(),
            workspace: None,
            context_filter: None,
            file_filter: None,
            kind_filter: Some(RepoGraphNodeKind::Route),
            edge_filter: vec![RepoGraphEdgeKind::HandlesRoute],
            token_budget: None,
            max_depth: Some(1),
            max_seeds: Some(4),
            min_hub_degree: Some(100),
        },
        None,
    );
    assert!(
        route_subgraph
            .nodes
            .iter()
            .any(|node| node.uid == "route:GET /api/helper (axum)")
    );
    assert_eq!(graph.node(tool).kind, RepoGraphNodeKind::Tool);
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
pub(super) fn definition_with_enclosing(
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
pub(super) fn nested_ranges_fixture() -> ParsedScipIndex {
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
