use super::*;
// PR C2: import the inner `ResolveOutcome` (NodeIndex) under the
// unqualified name so the existing test patterns keep compiling.
// The bridge crate's `ResolveOutcome` (String) is different — we
// never use it directly in these tests.
use crate::mcp_bridge::graph_neighbors::{ResolveOutcome, resolve_node, resolve_node_or_err};
use djinn_control_plane::bridge::{ComplexityMetrics as WireComplexityMetrics, ImpactEntry};
use djinn_graph::repo_graph::{RepoDependencyGraph, RepoNodeKey};
use djinn_graph::scip_parser::{
    ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
    ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

mod hotspots_churn;

/// Serialize tests that mutate `DJINN_CODE_GRAPH_AMBIGUITY` against
/// every other test that calls `resolve_node` — cargo runs tests in
/// parallel, so an env var set in one test would otherwise race with
/// peer threads reading it. The mutex is held for the duration of
/// the env mutation; tests that don't touch the env var still
/// acquire the lock so they can't see a transient `false`.
static AMBIGUITY_ENV_LOCK: Mutex<()> = Mutex::new(());

/// PR s6ch / 92z7: serialize tests that mutate `DJINN_ROUTE_PARITY`
/// against every other test that exercises
/// `route_parity_enabled`. Cargo runs tests in parallel, so an env
/// var set in one test would otherwise race with peer threads
/// reading it. Tests that don't touch the env var still acquire
/// the lock so they can't see a transient `false`.
pub(super) static ROUTE_PARITY_TEST_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that restores `DJINN_ROUTE_PARITY` to its previous
/// value on drop (including panic unwinds). Pair with
/// `ROUTE_PARITY_TEST_LOCK` so the mutation window can't race
/// peer tests.
pub(super) struct RouteParityGuard {
    prev: Option<String>,
}

impl RouteParityGuard {
    /// Set `DJINN_ROUTE_PARITY` to `value` and return a guard that
    /// will restore the prior env state on drop.
    pub(super) fn set(value: &str) -> Self {
        let prev = std::env::var(djinn_graph::route_extraction::ROUTE_PARITY_FLAG).ok();
        // SAFETY: callers pair this with `ROUTE_PARITY_TEST_LOCK`
        // so the env mutation can't race peer threads.
        unsafe {
            std::env::set_var(djinn_graph::route_extraction::ROUTE_PARITY_FLAG, value);
        }
        Self { prev }
    }
}

impl Drop for RouteParityGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(value) => unsafe {
                std::env::set_var(djinn_graph::route_extraction::ROUTE_PARITY_FLAG, value);
            },
            None => unsafe {
                std::env::remove_var(djinn_graph::route_extraction::ROUTE_PARITY_FLAG);
            },
        }
    }
}

fn fixture_index() -> ParsedScipIndex {
    let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
    let helper_symbol = ScipSymbol {
        symbol: helper_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("helper".to_string()),
        signature: Some("fn helper()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let trait_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("HelperTrait".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
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
        visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/helper.rs"),
                definitions: vec![ScipOccurrence {
                    symbol: helper_symbol_name.clone(),
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
                }],
                references: vec![],
                occurrences: vec![],
                symbols: vec![helper_symbol],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/app.rs"),
                definitions: vec![ScipOccurrence {
                    symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
                    range: ScipRange {
                        start_line: 0,
                        start_character: 0,
                        end_line: 0,
                        end_character: 4,
                    },
                    enclosing_range: None,
                    roles: BTreeSet::from([ScipSymbolRole::Definition]),
                    syntax_kind: None,
                    override_documentation: vec![],
                }],
                references: vec![ScipOccurrence {
                    symbol: helper_symbol_name,
                    range: ScipRange {
                        start_line: 1,
                        start_character: 4,
                        end_line: 1,
                        end_character: 10,
                    },
                    enclosing_range: None,
                    roles: BTreeSet::new(),
                    syntax_kind: None,
                    override_documentation: vec![],
                }],
                occurrences: vec![],
                symbols: vec![main_symbol, trait_symbol],
            },
        ],
        external_symbols: vec![],
    }
}
pub(crate) fn build_test_graph() -> RepoDependencyGraph {
    RepoDependencyGraph::build(&[fixture_index()])
}

fn multi_workspace_graph() -> RepoDependencyGraph {
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphEdgeKind,
        RepoGraphNode, RepoGraphNodeKind,
    };
    use djinn_graph::scip_parser::ScipVisibility;

    let mk_symbol =
        |symbol: &str, display_name: &str, file_path: &str, workspace: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(symbol.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: display_name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(file_path)),
            symbol: Some(symbol.to_string()),
            symbol_kind: Some(ScipSymbolKind::Function),
            is_external: false,
            visibility: Some(ScipVisibility::Public),
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some(workspace.to_string()),
            route_framework: None,
            route_handler_symbol: None,
        };
    let mk_edge = |source: usize, target: usize| RepoGraphArtifactEdge {
        source,
        target,
        kind: RepoGraphEdgeKind::SymbolReference,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.95,
        reason: None,
        step: None,
    };

    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            mk_symbol(
                "scip-rust pkg server/src/entry.rs `entry`().",
                "server_entry",
                "server/src/entry.rs",
                "server",
            ),
            mk_symbol(
                "scip-rust pkg desktop/src/shared.rs `shared`().",
                "desktop_shared",
                "desktop/src/shared.rs",
                "desktop",
            ),
            mk_symbol(
                "scip-rust pkg server/src/sink.rs `sink`().",
                "server_sink",
                "server/src/sink.rs",
                "server",
            ),
            mk_symbol(
                "scip-rust pkg desktop/src/leaf.rs `leaf`().",
                "desktop_leaf",
                "desktop/src/leaf.rs",
                "desktop",
            ),
        ],
        // The first two edges form a server → desktop → server chain.
        // Traversal ops should keep this cross-workspace middle hop when
        // the server workspace scopes only seed resolution.
        edges: vec![mk_edge(0, 1), mk_edge(1, 2), mk_edge(3, 1)],
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    };
    RepoDependencyGraph::from_artifact(&artifact)
}

#[test]
fn workspace_hint_for_nonexistent_slug_returns_available_slugs() {
    let graph = multi_workspace_graph();

    assert_eq!(
        shared::workspace_hint_from_graph(&graph, Some("nonexistent")),
        Some(vec!["desktop".to_string(), "server".to_string()])
    );
    assert_eq!(
        shared::workspace_hint_from_graph(&graph, Some("server")),
        None
    );
    assert_eq!(shared::workspace_hint_from_graph(&graph, Some("")), None);
}

#[test]
fn valid_workspace_restricts_listing_style_graph_ops() {
    let graph = multi_workspace_graph();
    let workspace = shared::active_workspace_prefix(&graph, Some("server"))
        .expect("server workspace should be active");

    let ranked_kept: Vec<String> = graph
        .rank()
        .nodes
        .iter()
        .filter_map(|ranked| {
            let node = graph.node(ranked.node_index);
            shared::repo_graph_node_matches_workspace(node, &workspace)
                .then(|| node.display_name.clone())
        })
        .collect();
    assert!(ranked_kept.iter().any(|name| name == "server_entry"));
    assert!(ranked_kept.iter().any(|name| name == "server_sink"));
    assert!(
        !ranked_kept.iter().any(|name| name.starts_with("desktop_")),
        "ranked hard filter leaked desktop nodes: {ranked_kept:?}"
    );

    let orphan_kept: Vec<String> = graph
        .orphans(None, None, usize::MAX)
        .into_iter()
        .filter_map(|idx| {
            let node = graph.node(idx);
            shared::repo_graph_node_matches_workspace(node, &workspace)
                .then(|| node.display_name.clone())
        })
        .collect();
    assert!(orphan_kept.iter().any(|name| name == "server_entry"));
    assert!(
        !orphan_kept.iter().any(|name| name.starts_with("desktop_")),
        "orphans hard filter leaked desktop nodes: {orphan_kept:?}"
    );

    let api_surface_kept: Vec<String> = graph
        .graph()
        .node_indices()
        .filter_map(|idx| {
            let node = graph.node(idx);
            (node.visibility == Some(djinn_graph::scip_parser::ScipVisibility::Public)
                && shared::repo_graph_node_matches_workspace(node, &workspace))
            .then(|| node.display_name.clone())
        })
        .collect();
    assert_eq!(api_surface_kept.len(), 2);
    assert!(
        api_surface_kept
            .iter()
            .all(|name| name.starts_with("server_"))
    );

    let snapshot_payload = build_snapshot_payload(
        &graph,
        &graph.rank(),
        "project".to_string(),
        "head".to_string(),
        "now".to_string(),
        &djinn_control_plane::tools::graph_exclusions::GraphExclusions::empty(),
        Some("server"),
        SnapshotLevel::Symbol,
        20,
    );
    assert_eq!(snapshot_payload.total_nodes, 2);
    assert!(
        snapshot_payload
            .nodes
            .iter()
            .all(|node| node.workspace.as_deref() == Some("server")),
        "snapshot hard filter leaked non-server nodes: {:?}",
        snapshot_payload.nodes
    );
}

#[test]
fn traversal_ops_scope_seeds_but_do_not_restrict_the_walk() {
    let graph = multi_workspace_graph();
    let entry = "scip-rust pkg server/src/entry.rs `entry`().";
    let shared = "scip-rust pkg desktop/src/shared.rs `shared`().";
    let sink = "scip-rust pkg server/src/sink.rs `sink`().";

    let sink_idx =
        shared::resolve_node_or_err_for_workspace_seed(&graph, sink, Some("server")).unwrap();
    let impact_keys: Vec<String> = shared::impact_bfs(&graph, sink_idx, 3, Some(0.0))
        .into_iter()
        .map(|(_, entry)| entry.key)
        .collect();
    assert!(
        impact_keys
            .iter()
            .any(|key| key.contains("desktop/src/shared.rs")),
        "impact should preserve cross-workspace blast radius after resolving a server seed: {impact_keys:?}"
    );

    let entry_idx =
        shared::resolve_node_or_err_for_workspace_seed(&graph, entry, Some("server")).unwrap();
    let path = graph
        .shortest_path(entry_idx, sink_idx, Some(4))
        .expect("server seeds should connect through desktop shared node");
    let path_keys: Vec<String> = path
        .iter()
        .map(|idx| format_node_key(&graph.node(*idx).id))
        .collect();
    assert!(
        path_keys
            .iter()
            .any(|key| key.contains("desktop/src/shared.rs")),
        "path should not hard-filter the desktop middle hop: {path_keys:?}"
    );

    let shared_idx = resolve_node_or_err(&graph, shared).unwrap();
    let touches_hot_path = path.contains(&shared_idx);
    assert!(
        touches_hot_path,
        "touches_hot_path semantics should allow an unscoped queried desktop symbol on a server seed path: {path_keys:?}"
    );

    assert!(
        shared::resolve_node_or_err_for_workspace_seed(&graph, shared, Some("server")).is_err(),
        "workspace scoping should apply to traversal seeds, even though the walk itself is unscoped"
    );
}

#[test]
fn resolve_node_finds_file_by_path() {
    let graph = build_test_graph();
    assert!(matches!(
        resolve_node(&graph, "src/app.rs"),
        ResolveOutcome::Found(_)
    ));
    assert!(matches!(
        resolve_node(&graph, "file:src/app.rs"),
        ResolveOutcome::Found(_)
    ));
}

#[test]
fn resolve_node_finds_symbol_by_name() {
    let graph = build_test_graph();
    assert!(matches!(
        resolve_node(&graph, "scip-rust pkg src/helper.rs `helper`()."),
        ResolveOutcome::Found(_)
    ));
    assert!(matches!(
        resolve_node(&graph, "symbol:scip-rust pkg src/helper.rs `helper`()."),
        ResolveOutcome::Found(_)
    ));
}

/// PR ga1k regression: `RepoGraphBridge::implementations` must accept
/// the canonical `symbol:<scip>`-prefixed key form that the MCP/chat
/// pre-resolver produces (see
/// `graph_ops::insights::resolve` returning
/// `ResolveOutcome::Found(format_node_key(&node.id))`) AND the bare
/// SCIP symbol, returning the same implementor list for both. The
/// fixture graph's `main → HelperTrait` SCIP `Implementation`
/// relationship is materialised as a `RepoGraphEdgeKind::Implements`
/// edge by `RepoDependencyGraph::build`, so the resolver points both
/// forms at the same trait node and the `implementations` op walks
/// the same incoming `Implements` edge from `main`.
#[test]
fn implementations_accepts_canonical_symbol_prefixed_key() {
    use crate::mcp_bridge::graph_ops::query::test_helpers::implementations_for_graph;
    let graph = build_test_graph();
    // The SCIP symbol for the trait — same string the fixture's
    // `HelperTrait` definition carries.
    let bare_scip = "scip-rust pkg src/types.rs `HelperTrait`#";
    let prefixed = format!("symbol:{bare_scip}");

    // Sanity: the resolver must resolve both forms to the same node.
    // The pre-fix op was failing exactly here with
    // `symbol 'symbol:…' not found in graph` because it bypassed the
    // shared resolver and called `graph.symbol_node` directly with
    // the bare SCIP symbol only.
    let bare_idx = resolve_node_or_err(&graph, bare_scip).unwrap();
    let prefixed_idx = resolve_node_or_err(&graph, &prefixed).unwrap();
    assert_eq!(
        bare_idx, prefixed_idx,
        "bare and canonical-prefixed forms must resolve to the same node"
    );

    // End-to-end: both call shapes return the same implementor list.
    let bare_impls = implementations_for_graph(&graph, bare_scip)
        .expect("bare SCIP symbol should resolve and find its implementors");
    let prefixed_impls = implementations_for_graph(&graph, &prefixed)
        .expect("symbol:-prefixed key should resolve and find its implementors");
    assert_eq!(
        bare_impls, prefixed_impls,
        "implementations must be invariant to the symbol: prefix"
    );
    // The fixture has exactly one implementor of `HelperTrait` —
    // `main` — and no external implementors, so the list must be
    // non-empty and contain the main symbol string.
    assert!(
        !bare_impls.is_empty(),
        "expected at least one implementor of HelperTrait in the fixture"
    );
    assert!(
        bare_impls
            .iter()
            .any(|s| s == "scip-rust pkg src/app.rs `main`()."),
        "expected `main` to be listed as an implementor of HelperTrait: {bare_impls:?}"
    );
}

#[test]
fn resolve_node_returns_not_found_for_unknown() {
    let graph = build_test_graph();
    // The fixture has `helper`/`main` and `HelperTrait` symbols, but
    // none with a name index entry for "totally_absent".
    assert!(matches!(
        resolve_node(&graph, "totally_absent_zzz"),
        ResolveOutcome::NotFound
    ));
}

/// Build a fixture with three distinct symbols all named `User`,
/// each in a different file. Used by the ambiguity / uid follow-up
/// / feature-flag tests below.
fn user_ambiguity_index() -> ParsedScipIndex {
    let mk_user_file = |path: &str, sym: &str, kind: ScipSymbolKind| ScipFile {
        language: "rust".to_string(),
        relative_path: PathBuf::from(path),
        definitions: vec![ScipOccurrence {
            symbol: sym.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 4,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }],
        references: vec![],
        occurrences: vec![],
        symbols: vec![ScipSymbol {
            symbol: sym.to_string(),
            kind: Some(kind),
            display_name: Some("User".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(djinn_graph::scip_parser::ScipVisibility::Public),
            signature_parts: None,
        }],
    };
    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            mk_user_file(
                "src/auth/User.rs",
                "scip-rust pkg src/auth/User.rs `User`#",
                ScipSymbolKind::Type,
            ),
            mk_user_file(
                "src/billing/Account.rs",
                "scip-rust pkg src/billing/Account.rs `User`#",
                ScipSymbolKind::Function,
            ),
            mk_user_file(
                "src/admin/Roles.rs",
                "scip-rust pkg src/admin/Roles.rs `User`#",
                ScipSymbolKind::Method,
            ),
        ],
        external_symbols: vec![],
    }
}

#[test]
fn resolve_node_returns_ambiguous_when_multi_match() {
    // Three distinct symbols share display name `User`. The
    // file-path-substring signal dominates the score formula, so
    // candidates whose path contains the lowercased query rank
    // ahead of the others. The fixture also yields a file node for
    // `src/auth/User.rs` (its relative path is its display name and
    // contains "user") — so the candidate count is 3 symbols + 1
    // file = 4. Cap at 8 per the C2 spec.
    let _guard = AMBIGUITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let graph = RepoDependencyGraph::build(&[user_ambiguity_index()]);
    let outcome = resolve_node(&graph, "User");
    match outcome {
        ResolveOutcome::Ambiguous(candidates) => {
            assert!(
                candidates.len() >= 3 && candidates.len() <= 8,
                "expected 3..=8 User candidates, got {}: {:?}",
                candidates.len(),
                candidates
            );
            assert!(
                candidates[0].file_path.to_lowercase().contains("user"),
                "highest-ranked candidate should match query in file path: {:?}",
                candidates
            );
            // Verify the three symbol-kind candidates are present.
            let symbol_count = candidates
                .iter()
                .filter(|c| c.uid.starts_with("symbol:"))
                .count();
            assert_eq!(symbol_count, 3, "expected exactly 3 symbol candidates");
        }
        ResolveOutcome::Found(_) => panic!("expected Ambiguous, got Found"),
        ResolveOutcome::NotFound => panic!("expected Ambiguous, got NotFound"),
    }
}

#[test]
fn resolve_node_after_uid_lookup_returns_unique() {
    // Once we have a candidate's `uid` (`"symbol:..."`), passing it
    // back as `key` resolves uniquely via the symbol index — that's
    // the C2 disambiguation handshake.
    let _guard = AMBIGUITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let graph = RepoDependencyGraph::build(&[user_ambiguity_index()]);
    let candidates = match resolve_node(&graph, "User") {
        ResolveOutcome::Ambiguous(c) => c,
        _ => panic!("expected Ambiguous"),
    };
    let uid = candidates[0].uid.clone();
    match resolve_node(&graph, &uid) {
        ResolveOutcome::Found(_) => {}
        _ => panic!("uid follow-up should resolve to Found"),
    }
}

#[test]
fn ambiguity_disabled_returns_not_found() {
    // With the feature flag off, a multi-match must collapse to
    // NotFound — preserving pre-PR-C2 semantics for callers that
    // haven't been updated to handle Ambiguous.
    //
    // SAFETY: env mutation races with parallel tests; AMBIGUITY_ENV_LOCK
    // serializes against every other resolver test in this module.
    let _guard = AMBIGUITY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let graph = RepoDependencyGraph::build(&[user_ambiguity_index()]);
    unsafe {
        std::env::set_var("DJINN_CODE_GRAPH_AMBIGUITY", "false");
    }
    let outcome = resolve_node(&graph, "User");
    unsafe {
        std::env::remove_var("DJINN_CODE_GRAPH_AMBIGUITY");
    }
    assert!(
        matches!(outcome, ResolveOutcome::NotFound),
        "with DJINN_CODE_GRAPH_AMBIGUITY=false a multi-match must collapse to NotFound"
    );
}

#[test]
fn score_formula_components() {
    // Verifies the C2 score formula:
    //   0.5 + 0.4*file_path_substring + 0.2*kind_hint + tiebreaker.
    // Spot-check a Type-kind node where both signals fire and the
    // tiebreaker contributes 0.05.
    use djinn_graph::repo_graph::*;
    use djinn_graph::scip_parser::ScipSymbolKind;
    use std::path::PathBuf;

    let node = RepoGraphNode {
        id: RepoNodeKey::Symbol("scip-rust pkg src/auth/User.rs `User`#".into()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: "User".into(),
        language: Some("rust".into()),
        file_path: Some(PathBuf::from("src/auth/User.rs")),
        symbol: Some("scip-rust pkg src/auth/User.rs `User`#".into()),
        symbol_kind: Some(ScipSymbolKind::Type),
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    };
    // Both file-path match (User in path) and kind hint ("class")
    // fire. Tiebreaker for Type/Class is 0.05.
    let s = crate::mcp_bridge::graph_neighbors::score_candidate(&node, "User", Some("class"));
    let expected = 0.5 + 0.4 * 1.0 + 0.2 * 1.0 + 0.05;
    assert!(
        (s - expected).abs() < 1e-9,
        "score {s} != expected {expected}"
    );

    // Same node, no kind hint: drop the 0.2 component.
    let s_no_hint = crate::mcp_bridge::graph_neighbors::score_candidate(&node, "User", None);
    let expected_no_hint = 0.5 + 0.4 * 1.0 + 0.05;
    assert!(
        (s_no_hint - expected_no_hint).abs() < 1e-9,
        "score {s_no_hint} != expected {expected_no_hint}"
    );

    // Query that doesn't appear in path: drop the 0.4 component.
    let s_no_path =
        crate::mcp_bridge::graph_neighbors::score_candidate(&node, "Account", Some("class"));
    let expected_no_path = 0.5 + 0.2 * 1.0 + 0.05;
    assert!(
        (s_no_path - expected_no_path).abs() < 1e-9,
        "score {s_no_path} != expected {expected_no_path}"
    );
}

#[test]
fn format_node_key_file() {
    let key = RepoNodeKey::File(PathBuf::from("src/lib.rs"));
    assert_eq!(format_node_key(&key), "file:src/lib.rs");
}

#[test]
fn format_node_key_symbol() {
    let key = RepoNodeKey::Symbol("scip-rust . . . Foo#".to_string());
    assert_eq!(format_node_key(&key), "symbol:scip-rust . . . Foo#");
}

#[tokio::test]
async fn neighbors_returns_connected_nodes() {
    let graph = build_test_graph();
    let node_index = match resolve_node(&graph, "src/app.rs") {
        ResolveOutcome::Found(idx) => idx,
        _ => panic!("expected Found"),
    };
    let mut neighbors = Vec::new();
    for dir in [petgraph::Direction::Incoming, petgraph::Direction::Outgoing] {
        let dir_label = match dir {
            petgraph::Direction::Incoming => "incoming",
            petgraph::Direction::Outgoing => "outgoing",
        };
        for edge in graph.graph().edges_directed(node_index, dir) {
            let other_index = match dir {
                petgraph::Direction::Outgoing => edge.target(),
                petgraph::Direction::Incoming => edge.source(),
            };
            let other_node = graph.node(other_index);
            neighbors.push(GraphNeighbor {
                key: format_node_key(&other_node.id),
                uid: format_node_key(&other_node.id),
                kind: format!("{:?}", other_node.kind).to_lowercase(),
                display_name: other_node.display_name.clone(),
                edge_kind: format!("{:?}", edge.weight().kind),
                edge_weight: edge.weight().weight,
                direction: dir_label.to_string(),
            });
        }
    }
    assert!(
        !neighbors.is_empty(),
        "expected at least one neighbor for src/app.rs"
    );
    assert!(neighbors.iter().any(|n| n.display_name == "helper"));
}

#[tokio::test]
async fn ranked_returns_scored_nodes() {
    let graph = build_test_graph();
    let ranking = graph.rank();
    let nodes: Vec<RankedNode> = ranking
        .nodes
        .iter()
        .take(5)
        .map(|node| {
            let graph_node = graph.node(node.node_index);
            RankedNode {
                key: format_node_key(&node.key),
                uid: format_node_key(&node.key),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: graph_node.display_name.clone(),
                score: node.score,
                page_rank: node.page_rank,
                structural_weight: node.structural_weight,
                inbound_edge_weight: node.inbound_edge_weight,
                outbound_edge_weight: node.outbound_edge_weight,
                process_id: None,
                community_id: None,
                is_entry_point: node.is_entry_point,
                entry_point_distance: node.entry_point_distance,
            }
        })
        .collect();
    assert!(!nodes.is_empty());
    for node in &nodes {
        assert!(node.score >= 0.0);
    }
}

/// PR F4: build a graph with a `tests/**`-shadowed file and assert
/// the post-exclusion `ranked` projection (the same filter the
/// bridge applies in [`RepoGraphBridge::ranked`]) drops it. We
/// exercise the predicate inline rather than spinning up the full
/// async bridge — a DB-backed AppState would dominate the test
/// runtime without adding signal.
#[test]
fn ranked_respects_graph_exclusions() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::repo_graph::RepoDependencyGraph;

    // Promote a fixture file into `tests/` so the glob matches.
    let mut idx = fixture_index();
    idx.files[0].relative_path = PathBuf::from("tests/helper.rs");
    let graph = RepoDependencyGraph::build(&[idx]);
    let ranking = graph.rank();
    let exclusions = GraphExclusions::build(&["tests/**".to_string()], &[]);

    let kept: Vec<String> = ranking
        .nodes
        .iter()
        .filter_map(|node| {
            let g = graph.node(node.node_index);
            let key = format_node_key(&node.key);
            let file = g.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&key, file.as_deref(), &g.display_name) {
                return None;
            }
            Some(key)
        })
        .collect();

    assert!(
        !kept.iter().any(|k| k.contains("tests/helper.rs")),
        "tests/helper.rs leaked through GraphExclusions: {kept:?}",
    );
}

/// PR F4: same as `ranked_respects_graph_exclusions` but for the
/// search code path.
#[test]
fn search_respects_graph_exclusions() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::repo_graph::RepoDependencyGraph;

    let mut idx = fixture_index();
    idx.files[0].relative_path = PathBuf::from("tests/helper.rs");
    let graph = RepoDependencyGraph::build(&[idx]);
    let exclusions = GraphExclusions::build(&["tests/**".to_string()], &[]);

    let hits = graph.search_by_name("helper", None, usize::MAX);
    let mut kept: Vec<String> = Vec::new();
    for hit in hits {
        let node = graph.node(hit.node_index);
        let key = format_node_key(&node.id);
        let file = node.file_path.as_ref().map(|p| p.display().to_string());
        if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
            continue;
        }
        kept.push(key);
    }
    assert!(
        !kept.iter().any(|k| k.contains("tests/helper.rs")),
        "tests/helper.rs leaked through search exclusions: {kept:?}",
    );
}

/// PR F4: with the new fused-rank default, an entry-point function
/// (the fixture's `fn main`, picked up by the entry-point detector)
/// must rank above a generic helper symbol. Before the multi-signal
/// fusion landed, `helper` outranked `main` because it had a
/// fan-in via `FileReference` from `src/app.rs`.
///
/// We do NOT assert a strict main-outranks-helper position on this
/// fixture: with only one caller-callee pair the entry-point
/// distance signal is too weak to break a 2-out-of-3 RRF vote in
/// helper's favour. The peer
/// `rrf_fused_rank_promotes_entry_points_under_pagerank_tie`
/// test in `repo_graph::tests` exercises the lift in isolation.
#[test]
fn ranked_default_sort_is_fused_and_promotes_entry_points() {
    use djinn_graph::repo_graph::{RepoDependencyGraph, RepoNodeKey};
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();

    let main_node = ranking
        .nodes
        .iter()
        .find(|node| {
            node.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string())
        })
        .expect("main symbol should be ranked");

    // The detector tagged `main` as an entry point, so the
    // side-channel that drives UI bucketing must reflect that.
    assert!(
        main_node.is_entry_point,
        "expected `main` to be marked as an entry point",
    );
    assert_eq!(
        main_node.entry_point_distance,
        Some(0),
        "entry-point function should sit at distance 0",
    );

    // Fused rank is the active sort signal: every adjacent pair
    // in the ranking is fused-rank-monotonic.
    for window in ranking.nodes.windows(2) {
        assert!(
            window[0].fused_rank >= window[1].fused_rank,
            "ranking not fused-rank-desc: {} < {} (keys {:?} vs {:?})",
            window[0].fused_rank,
            window[1].fused_rank,
            window[0].key,
            window[1].key,
        );
    }
}

#[tokio::test]
async fn implementations_finds_implementors() {
    let graph = build_test_graph();
    let trait_symbol = "scip-rust pkg src/types.rs `HelperTrait`#";
    let node_index = graph
        .symbol_node(trait_symbol)
        .expect("trait symbol should exist");
    let mut impls = Vec::new();
    for edge in graph
        .graph()
        .edges_directed(node_index, petgraph::Direction::Incoming)
    {
        if edge.weight().kind == djinn_graph::repo_graph::RepoGraphEdgeKind::Implements {
            let source_node = graph.node(edge.source());
            if let Some(sym) = &source_node.symbol {
                impls.push(sym.clone());
            }
        }
    }
    assert_eq!(impls.len(), 1);
    assert!(impls[0].contains("main"));
}

#[tokio::test]
async fn impact_returns_transitive_dependents() {
    let graph = build_test_graph();
    let start = match resolve_node(&graph, "scip-rust pkg src/helper.rs `helper`().") {
        ResolveOutcome::Found(idx) => idx,
        _ => panic!("expected Found"),
    };
    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start, 0usize));
    let mut result = Vec::new();
    let max_depth = 3;

    while let Some((current, depth)) = queue.pop_front() {
        if depth > 0 {
            let node = graph.node(current);
            result.push(ImpactEntry {
                uid: node.stable_uid(),
                key: format_node_key(&node.id),
                depth,
                file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
                confidence_tier: None,
                exclusion_reason: None,
            });
        }
        if depth < max_depth {
            for edge in graph
                .graph()
                .edges_directed(current, petgraph::Direction::Incoming)
            {
                let source = edge.source();
                if visited.insert(source) {
                    queue.push_back((source, depth + 1));
                }
            }
        }
    }
    assert!(
        !result.is_empty(),
        "expected at least one node in the impact set"
    );
}

/// v8: `impact_bfs` skips structural anchors (`ContainsDefinition`,
/// `DeclaredInFile`) and synthetic side-channels (`MemberOf`,
/// `StepInProcess`, `EntryPointOf`) so an impact walk doesn't
/// pull in "every node that's anchored to this file". The
/// behavioral set (`Reads`/`Writes`/`SymbolReference`/`FileReference`
/// /typing relationships) IS walked.
///
/// Build a tiny graph with one structural and one behavioral
/// incoming edge to a target node, run impact_bfs, assert the
/// behavioral source is admitted and the structural source is
/// not.
#[tokio::test]
async fn impact_bfs_skips_structural_anchors_but_walks_behavioral_edges() {
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
        RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    };

    let mk_node = |key: RepoNodeKey, name: &str, kind: RepoGraphNodeKind| RepoGraphNode {
        id: key.clone(),
        kind,
        display_name: name.to_string(),
        language: None,
        file_path: None,
        symbol: None,
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    };
    // Three nodes:
    //   [0] target — receives both edges
    //   [1] behavioral_src → target via Reads (should propagate)
    //   [2] structural_src → target via ContainsDefinition (should NOT)
    let nodes = vec![
        mk_node(
            RepoNodeKey::Symbol("symbol:target".to_string()),
            "target",
            RepoGraphNodeKind::Symbol,
        ),
        mk_node(
            RepoNodeKey::Symbol("symbol:behavioral".to_string()),
            "behavioral_caller",
            RepoGraphNodeKind::Symbol,
        ),
        mk_node(
            RepoNodeKey::File(std::path::PathBuf::from("src/foo.rs")),
            "src/foo.rs",
            RepoGraphNodeKind::File,
        ),
    ];
    let mk_edge = |source: usize, target: usize, kind: RepoGraphEdgeKind| RepoGraphArtifactEdge {
        source,
        target,
        kind,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.95,
        reason: None,
        step: None,
    };
    let edges = vec![
        mk_edge(1, 0, RepoGraphEdgeKind::Reads),
        mk_edge(2, 0, RepoGraphEdgeKind::ContainsDefinition),
    ];
    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    };
    let graph = RepoDependencyGraph::from_artifact(&artifact);
    let target_idx = graph
        .symbol_node("symbol:target")
        .expect("target should resolve");

    let result = shared::impact_bfs(&graph, target_idx, 3, Some(0.0));
    let keys: Vec<&str> = result.iter().map(|(_, e)| e.key.as_str()).collect();
    assert!(
        keys.iter().any(|k| k.contains("symbol:behavioral")),
        "behavioral Reads edge should propagate; got {keys:?}"
    );
    assert!(
        !keys.iter().any(|k| k.contains("src/foo.rs")),
        "structural ContainsDefinition edge should NOT propagate; got {keys:?}"
    );
}

/// v8: `impact_bfs` defaults `min_confidence` to 0.85 when the
/// caller passes `None`. Floor above the highest possible
/// confidence (1.0+) collapses the frontier to empty regardless
/// of edge kind.
#[tokio::test]
async fn impact_bfs_min_confidence_default_and_strict_threshold() {
    let graph = build_test_graph();
    let helper_idx =
        resolve_node_or_err(&graph, "scip-rust pkg src/helper.rs `helper`().").unwrap();

    // A floor above the highest possible confidence drops
    // everything — proves the threshold is honored.
    let strict = shared::impact_bfs(&graph, helper_idx, 3, Some(1.5));
    assert!(
        strict.is_empty(),
        "min_confidence above 1.0 must collapse the frontier to empty"
    );

    // Default (None → 0.85) admits high-confidence FileReference
    // edges (floor 0.85) and Reads/Writes/SymbolReference (0.85+)
    // — fixture's app.rs ↔ helper.rs FileReference at 0.85
    // qualifies, so default-walked result contains the helper
    // file's caller file.
    let with_default = shared::impact_bfs(&graph, helper_idx, 3, None);
    let keys: Vec<&str> = with_default.iter().map(|(_, e)| e.key.as_str()).collect();
    assert!(
        keys.iter().any(|k| k.contains("src/app.rs")),
        "default 0.85 floor should still admit the file→file FileReference edge \
         (app.rs references helper.rs); got {keys:?}"
    );
}

/// PR A2: `min_confidence` on the BFS frontier drops weak edges. A
/// threshold above the highest confidence in the fixture must collapse
/// the impact set to empty; mid-band thresholds must shrink it.
/// We replicate the impact BFS inline (the production handler is async
/// and needs an `MCPBridge`/db, neither cheap to spin up here).
#[tokio::test]
async fn impact_min_confidence_filters_bfs_frontier_pr_a2() {
    let graph = build_test_graph();
    let start = resolve_node_or_err(&graph, "scip-rust pkg src/helper.rs `helper`().").unwrap();

    fn run_bfs(
        graph: &djinn_graph::repo_graph::RepoDependencyGraph,
        start: petgraph::graph::NodeIndex,
        max_depth: usize,
        min_confidence: Option<f64>,
    ) -> usize {
        let mut visited = std::collections::HashSet::new();
        visited.insert(start);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((start, 0usize));
        let mut count = 0;
        while let Some((current, depth)) = queue.pop_front() {
            if depth > 0 {
                count += 1;
            }
            if depth < max_depth {
                for edge in graph
                    .graph()
                    .edges_directed(current, petgraph::Direction::Incoming)
                {
                    if let Some(threshold) = min_confidence
                        && edge.weight().confidence < threshold
                    {
                        continue;
                    }
                    let source = edge.source();
                    if visited.insert(source) {
                        queue.push_back((source, depth + 1));
                    }
                }
            }
        }
        count
    }

    let unfiltered = run_bfs(&graph, start, 3, None);
    assert!(unfiltered > 0, "fixture must yield a non-empty impact set");

    // Threshold above 1.0 collapses the frontier to empty.
    let strict = run_bfs(&graph, start, 3, Some(1.5));
    assert_eq!(
        strict, 0,
        "min_confidence=1.5 must drop every edge — got {strict} entries"
    );

    // A modest threshold must not exceed the unfiltered count and may
    // shrink it.
    let mid = run_bfs(&graph, start, 3, Some(0.85));
    assert!(
        mid <= unfiltered,
        "filtered count {mid} must be <= unfiltered {unfiltered}"
    );
}

// ── PR C1: `context` op tests ────────────────────────────────────

/// Builds a synthetic graph and returns
///   (graph, helper_node_index, helper_uid_string)
/// — used by the C1 tests below so they don't repeat the setup.
fn build_context_fixture() -> (
    djinn_graph::repo_graph::RepoDependencyGraph,
    petgraph::graph::NodeIndex,
    String,
) {
    let graph = build_test_graph();
    let key = "scip-rust pkg src/helper.rs `helper`().";
    let node_index = match resolve_node(&graph, key) {
        ResolveOutcome::Found(idx) => idx,
        _ => panic!("expected helper symbol in fixture"),
    };
    (graph, node_index, key.to_string())
}

/// Replicates the production `context()` bucketing logic without
/// spinning up an `MCPBridge`/db. Returns the populated maps so we
/// can assert against them directly.
fn collect_context_buckets(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    node_index: petgraph::graph::NodeIndex,
) -> (
    std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>>,
    std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>>,
) {
    use crate::mcp_bridge::graph_neighbors::{build_related_symbol, edge_category_for};
    use petgraph::Direction;
    let mut incoming: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
        std::collections::BTreeMap::new();
    let mut outgoing: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
        std::collections::BTreeMap::new();
    for dir in [Direction::Incoming, Direction::Outgoing] {
        for edge in graph.graph().edges_directed(node_index, dir) {
            let other_index = match dir {
                Direction::Incoming => edge.source(),
                Direction::Outgoing => edge.target(),
            };
            let other = graph.node(other_index);
            let cat = edge_category_for(Some(edge.weight()), other);
            let related = build_related_symbol(other, edge.weight().confidence);
            let bucket = match dir {
                Direction::Incoming => incoming.entry(cat).or_default(),
                Direction::Outgoing => outgoing.entry(cat).or_default(),
            };
            bucket.push(related);
        }
    }
    for buckets in [&mut incoming, &mut outgoing] {
        for entries in buckets.values_mut() {
            entries.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.uid.cmp(&b.uid))
            });
            entries.truncate(30);
        }
    }
    (incoming, outgoing)
}

#[tokio::test]
async fn context_buckets_match_neighbors_count_pr_c1() {
    // Plan acceptance: `incoming.calls.len()` (and the union over
    // every bucket) must equal what a sibling `neighbors` call
    // returns for the same node. Rebuild the neighbors() count
    // inline to keep the assertion graph-only.
    use petgraph::Direction;
    let (graph, node_index, _) = build_context_fixture();
    let (incoming, outgoing) = collect_context_buckets(&graph, node_index);

    let incoming_total: usize = incoming.values().map(|v| v.len()).sum();
    let outgoing_total: usize = outgoing.values().map(|v| v.len()).sum();

    let raw_incoming = graph
        .graph()
        .edges_directed(node_index, Direction::Incoming)
        .count();
    let raw_outgoing = graph
        .graph()
        .edges_directed(node_index, Direction::Outgoing)
        .count();

    // `helper` has at most 30 incoming/outgoing in the synthetic
    // fixture; with the hard cap not engaging, the bucketed total
    // must equal the raw edge count.
    assert!(
        raw_incoming <= 30,
        "fixture has too many incoming edges; widen the test"
    );
    assert!(
        raw_outgoing <= 30,
        "fixture has too many outgoing edges; widen the test"
    );
    assert_eq!(
        incoming_total, raw_incoming,
        "context.incoming bucket sum {incoming_total} != raw neighbors {raw_incoming}"
    );
    assert_eq!(
        outgoing_total, raw_outgoing,
        "context.outgoing bucket sum {outgoing_total} != raw neighbors {raw_outgoing}"
    );
}

#[tokio::test]
async fn context_relationship_bucket_implements_pr_c1() {
    // The fixture wires `main` → `HelperTrait` via a SCIP
    // `is_implementation=true` relationship, which the
    // `RepoGraphEdgeKind::Implements` → `EdgeCategory::Implements`
    // mapping must surface in the outgoing.implements bucket.
    let graph = build_test_graph();
    let main_index = match resolve_node(&graph, "scip-rust pkg src/app.rs `main`().") {
        ResolveOutcome::Found(idx) => idx,
        _ => panic!("expected main symbol"),
    };
    let (_, outgoing) = collect_context_buckets(&graph, main_index);

    let implements = outgoing
        .get(&EdgeCategory::Implements)
        .cloned()
        .unwrap_or_default();
    assert!(
        implements.iter().any(|r| r.name.contains("HelperTrait")),
        "expected HelperTrait in outgoing.implements: {implements:?}"
    );
    // Confirm Extends bucket is *empty* — the fixture's relationship
    // only sets `is_implementation`, not `is_reference`.
    assert!(
        outgoing
            .get(&EdgeCategory::Extends)
            .is_none_or(|v| v.is_empty()),
        "outgoing.extends should be empty when only is_implementation is set"
    );
}

#[tokio::test]
async fn context_imports_bucket_for_file_references_pr_c1() {
    // FileReference edges (file → symbol or file → file) land in
    // the Imports bucket. The fixture's `src/app.rs` references
    // the `helper` symbol, so we expect `helper` in
    // `src/app.rs`'s outgoing.imports.
    let graph = build_test_graph();
    let app_index = match resolve_node(&graph, "src/app.rs") {
        ResolveOutcome::Found(idx) => idx,
        _ => panic!("expected src/app.rs file node"),
    };
    let (_, outgoing) = collect_context_buckets(&graph, app_index);

    let imports = outgoing
        .get(&EdgeCategory::Imports)
        .cloned()
        .unwrap_or_default();
    assert!(
        imports.iter().any(|r| r.name == "helper"),
        "expected `helper` in src/app.rs outgoing.imports: {imports:?}"
    );
}

mod complexity_refactor;
mod crate_graph;
mod flow;
mod registry_bridge_coverage;
mod snapshot;
mod snapshot_edge_cap;
mod trait_dispatch_query;

mod route;
mod trait_dispatch_corpus;
mod trait_dispatch_corpus_e2e;
mod trait_dispatch_impact;
