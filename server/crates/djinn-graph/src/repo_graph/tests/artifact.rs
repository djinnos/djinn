// djinn:allow-oversize — legacy test module over size-guard threshold; split when touched substantively.

use super::*;

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
fn route_and_tool_nodes_round_trip_with_metadata() {
    let mut graph = RepoDependencyGraph::build(&[]);

    let route = graph.ensure_route_node(
        "GET /api/agents (axum)",
        "GET /api/agents (axum)",
        Some("rust"),
        Some("api"),
        Some(Path::new("src/routes/agents.rs")),
        Some("axum"),
        Some("scip-rust pkg src/routes/agents.rs `list_agents`()."),
    );
    let tool = graph.ensure_tool_node(
        "agents.list",
        "agents.list",
        Some("rust"),
        Some("api"),
        Some(Path::new("src/tools/agents.rs")),
    );

    let route_node = graph.node(route);
    assert_eq!(
        route_node.id,
        RepoNodeKey::Route("GET /api/agents (axum) @ src/routes/agents.rs".to_string())
    );
    assert_eq!(route_node.kind, RepoGraphNodeKind::Route);
    assert_eq!(route_node.display_name, "GET /api/agents (axum)");
    assert_eq!(route_node.language.as_deref(), Some("rust"));
    assert_eq!(route_node.workspace.as_deref(), Some("api"));
    assert_eq!(route_node.route_framework.as_deref(), Some("axum"));
    assert_eq!(
        route_node.route_handler_symbol.as_deref(),
        Some("scip-rust pkg src/routes/agents.rs `list_agents`().")
    );

    let tool_node = graph.node(tool);
    assert_eq!(
        tool_node.id,
        RepoNodeKey::Tool("agents.list @ src/tools/agents.rs [workspace=api]".to_string())
    );
    assert_eq!(tool_node.kind, RepoGraphNodeKind::Tool);
    assert_eq!(tool_node.display_name, "agents.list");
    assert_eq!(tool_node.language.as_deref(), Some("rust"));
    assert_eq!(tool_node.workspace.as_deref(), Some("api"));
    assert_eq!(tool_node.route_framework, None);
    assert_eq!(tool_node.route_handler_symbol, None);

    let json = graph
        .serialize_artifact()
        .expect("serialize route/tool graph");
    assert!(json.contains("\"kind\":\"route\""));
    assert!(json.contains("\"kind\":\"tool\""));
    assert!(json.contains("\"route_framework\":\"axum\""));
    assert!(json.contains("\"route_handler_symbol\""));

    let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize");
    let restored_route = restored
        .node_lookup
        .get(&RepoNodeKey::Route(
            "GET /api/agents (axum) @ src/routes/agents.rs".to_string(),
        ))
        .copied()
        .expect("route lookup should survive artifact round trip");
    let restored_tool = restored
        .node_lookup
        .get(&RepoNodeKey::Tool(
            "agents.list @ src/tools/agents.rs [workspace=api]".to_string(),
        ))
        .copied()
        .expect("tool lookup should survive artifact round trip");

    assert_eq!(
        restored.node(restored_route).route_framework.as_deref(),
        Some("axum")
    );
    assert_eq!(
        restored
            .node(restored_route)
            .route_handler_symbol
            .as_deref(),
        Some("scip-rust pkg src/routes/agents.rs `list_agents`().")
    );
    assert_eq!(restored.node(restored_tool).kind, RepoGraphNodeKind::Tool);
}

#[test]
fn mixed_route_tool_artifact_bincode_round_trip_preserves_route_edges() {
    let mut graph = RepoDependencyGraph::build(&[fixture_index()]);
    let handler_symbol = "scip-rust pkg src/helper.rs `helper`().";
    let caller_symbol = "scip-rust pkg src/app.rs `main`().";

    let route = graph.ensure_route_node(
        "GET /api/helper (axum)",
        "GET /api/helper (axum)",
        Some("rust"),
        Some("api"),
        Some(Path::new("src/helper.rs")),
        Some("axum"),
        Some(handler_symbol),
    );
    let _tool = graph.ensure_tool_node("helper.run", "helper.run", Some("rust"), Some("api"), None);
    let handler = graph
        .symbol_node(handler_symbol)
        .expect("fixture helper symbol");
    let caller = graph
        .symbol_node(caller_symbol)
        .expect("fixture main symbol");

    graph.add_handles_route_edge(route, handler, "axum-route-attr", Some(0.87));
    graph.add_fetches_edge(caller, route, "ts-fetch-literal", Some(0.42));

    let artifact = graph.to_artifact();
    assert_eq!(artifact.version, REPO_GRAPH_ARTIFACT_VERSION);
    assert!(
        artifact
            .nodes
            .iter()
            .any(|node| node.kind == RepoGraphNodeKind::File)
    );
    assert!(
        artifact
            .nodes
            .iter()
            .any(|node| node.kind == RepoGraphNodeKind::Symbol)
    );
    assert!(
        artifact
            .nodes
            .iter()
            .any(|node| node.kind == RepoGraphNodeKind::Route)
    );
    assert!(
        artifact
            .nodes
            .iter()
            .any(|node| node.kind == RepoGraphNodeKind::Tool)
    );
    assert!(
        artifact
            .edges
            .iter()
            .any(|edge| edge.kind == RepoGraphEdgeKind::ContainsDefinition)
    );
    assert!(
        artifact
            .edges
            .iter()
            .any(|edge| edge.kind == RepoGraphEdgeKind::HandlesRoute)
    );
    assert!(
        artifact
            .edges
            .iter()
            .any(|edge| edge.kind == RepoGraphEdgeKind::Fetches)
    );

    let encoded = bincode::serialize(&artifact).expect("bincode serialize mixed graph");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize mixed graph through artifact compatibility path");
    let restored = RepoDependencyGraph::from_artifact(&decoded);

    let restored_route = restored
        .graph()
        .node_indices()
        .find(|&idx| {
            let node = restored.node(idx);
            node.kind == RepoGraphNodeKind::Route && node.display_name == "GET /api/helper (axum)"
        })
        .expect("route node survives bincode round trip");
    let restored_tool = restored
        .graph()
        .node_indices()
        .find(|&idx| {
            let node = restored.node(idx);
            node.kind == RepoGraphNodeKind::Tool && node.display_name == "helper.run"
        })
        .expect("tool node survives bincode round trip");
    let restored_handler = restored
        .symbol_node(handler_symbol)
        .expect("handler symbol survives bincode round trip");
    let restored_caller = restored
        .symbol_node(caller_symbol)
        .expect("caller symbol survives bincode round trip");

    assert_eq!(restored.node(restored_route).kind, RepoGraphNodeKind::Route);
    assert_eq!(restored.node(restored_tool).kind, RepoGraphNodeKind::Tool);

    let handles_route = restored
        .graph()
        .edges_connecting(restored_route, restored_handler)
        .find(|edge| edge.weight().kind == RepoGraphEdgeKind::HandlesRoute)
        .expect("HandlesRoute edge survives bincode round trip")
        .weight();
    assert_eq!(handles_route.reason.as_deref(), Some("axum-route-attr"));
    assert!((handles_route.confidence - 0.87).abs() < 1e-9);

    let fetches = restored
        .graph()
        .edges_connecting(restored_caller, restored_route)
        .find(|edge| edge.weight().kind == RepoGraphEdgeKind::Fetches)
        .expect("Fetches edge survives bincode round trip")
        .weight();
    assert_eq!(fetches.reason.as_deref(), Some("ts-fetch-literal"));
    assert!((fetches.confidence - 0.42).abs() < 1e-9);
}

#[test]
fn json_route_tool_and_route_edge_names_deserialize_with_node_field_defaults() {
    let json = format!(
        r#"{{
            "version": {version},
            "nodes": [
                {{
                    "id": {{ "Route": "GET /api/helper (axum)" }},
                    "kind": "route",
                    "display_name": "GET /api/helper (axum)",
                    "language": "rust",
                    "file_path": null,
                    "symbol": null,
                    "symbol_kind": null,
                    "is_external": false,
                    "visibility": null,
                    "signature": null,
                    "documentation": [],
                    "signature_parts": null,
                    "is_test": false,
                    "complexity": null,
                    "workspace": "api"
                }},
                {{
                    "id": {{ "Tool": "helper.run" }},
                    "kind": "tool",
                    "display_name": "helper.run",
                    "language": "rust",
                    "file_path": null,
                    "symbol": null,
                    "symbol_kind": null,
                    "is_external": false,
                    "visibility": null,
                    "signature": null,
                    "documentation": [],
                    "signature_parts": null,
                    "is_test": false,
                    "complexity": null,
                    "workspace": "api"
                }},
                {{
                    "id": {{ "Symbol": "scip-rust pkg src/helper.rs `helper`()." }},
                    "kind": "symbol",
                    "display_name": "helper",
                    "language": "rust",
                    "file_path": "src/helper.rs",
                    "symbol": "scip-rust pkg src/helper.rs `helper`().",
                    "symbol_kind": "Function",
                    "is_external": false,
                    "visibility": null,
                    "signature": null,
                    "documentation": [],
                    "signature_parts": null,
                    "is_test": false,
                    "complexity": null,
                    "workspace": "api"
                }}
            ],
            "edges": [
                {{
                    "source": 0,
                    "target": 2,
                    "kind": "handles_route",
                    "weight": 2.0,
                    "evidence_count": 1,
                    "confidence": 0.91,
                    "reason": "axum-route-attr"
                }},
                {{
                    "source": 2,
                    "target": 0,
                    "kind": "fetches",
                    "weight": 0.4,
                    "evidence_count": 1,
                    "confidence": 0.55,
                    "reason": "ts-fetch-literal"
                }}
            ],
            "symbol_ranges": {{}},
            "communities": [],
            "processes": []
        }}"#,
        version = REPO_GRAPH_ARTIFACT_VERSION
    );

    let artifact: RepoGraphArtifact = serde_json::from_str(&json).expect("deserialize route JSON");
    assert_eq!(artifact.nodes[0].kind, RepoGraphNodeKind::Route);
    assert_eq!(artifact.nodes[1].kind, RepoGraphNodeKind::Tool);
    assert_eq!(artifact.nodes[0].route_framework, None);
    assert_eq!(artifact.nodes[0].route_handler_symbol, None);
    assert_eq!(artifact.nodes[1].route_framework, None);
    assert_eq!(artifact.nodes[1].route_handler_symbol, None);
    assert_eq!(artifact.edges[0].kind, RepoGraphEdgeKind::HandlesRoute);
    assert_eq!(artifact.edges[0].reason.as_deref(), Some("axum-route-attr"));
    assert!((artifact.edges[0].confidence - 0.91).abs() < 1e-9);
    assert_eq!(artifact.edges[1].kind, RepoGraphEdgeKind::Fetches);
    assert_eq!(
        artifact.edges[1].reason.as_deref(),
        Some("ts-fetch-literal")
    );
    assert!((artifact.edges[1].confidence - 0.55).abs() < 1e-9);

    let restored = RepoDependencyGraph::from_artifact(&artifact);
    assert_eq!(restored.node_count(), 3);
    assert_eq!(restored.edge_count(), 2);
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
        route_exclusion_config: RouteExclusionConfig::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    let json = serde_json::to_string(&empty).expect("serialize empty");
    let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize empty");
    assert_eq!(restored.node_count(), 0);
    assert_eq!(restored.edge_count(), 0);
    // PR s6ch / 92z7: freshly deserialized empty graphs inherit
    // the baseline route-exclusion config so the bridge can read
    // it without an extra round-trip.
    assert_eq!(
        restored.route_exclusion_config(),
        &RouteExclusionConfig::default()
    );
}

#[test]
fn cochange_edges_round_trip_through_shared_edges_vec_and_stay_out_of_petgraph() {
    use crate::cochange::{CoChangeInput, coupling_score, derive_cochange_edges};

    fn file_node(path: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::File(PathBuf::from(path)),
            kind: RepoGraphNodeKind::File,
            display_name: path.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(path)),
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: None,
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    // Two file nodes, no SCIP edges.
    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![file_node("src/a.rs"), file_node("crates/other/src/b.rs")],
        edges: vec![],
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: vec![],
        route_exclusion_config: RouteExclusionConfig::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    let mut graph = RepoDependencyGraph::from_artifact(&artifact);
    // A legacy-shaped blob (no co-change edges) hydrates an empty sidecar.
    assert!(graph.cochange_edges().is_empty());

    // Materialize a co-change edge from a coupling-index row.
    let inputs = vec![CoChangeInput {
        file_a: "src/a.rs".to_string(),
        file_b: "crates/other/src/b.rs".to_string(),
        co_changes: 10,
        last_co_change_iso: "2026-07-15T12:00:00.000Z".to_string(),
    }];
    let derived = derive_cochange_edges(&graph, &inputs);
    assert_eq!(derived.len(), 1, "one qualifying pair → one edge");
    graph.set_cochange_edges(derived);

    // Co-change edges never enter the petgraph.
    assert_eq!(graph.edge_count(), 0, "co-change must not pollute petgraph");

    // Serialize → bincode → deserialize via the compat seam → rebuild.
    let out = graph.to_artifact();
    assert!(
        out.edges
            .iter()
            .any(|e| e.kind == RepoGraphEdgeKind::CoChangedWith),
        "co-change edge persists in the shared edges vec"
    );
    let encoded = bincode::serialize(&out).expect("serialize cochange artifact");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize through artifact compatibility path");
    let restored = RepoDependencyGraph::from_artifact(&decoded);

    assert_eq!(
        restored.edge_count(),
        0,
        "co-change stays out of the petgraph after reload"
    );
    let cc = restored.cochange_edges();
    assert_eq!(cc.len(), 1);
    assert_eq!(cc[0].evidence_count, 10);
    assert!((cc[0].confidence - coupling_score(10)).abs() < 1e-9);
    assert!(cc[0].last_co_change > 20_000, "temporal epoch day survived");
}

#[test]
fn from_artifact_round_trips_route_exclusion_config() {
    // PR s6ch / 92z7: the in-memory graph must carry the
    // `route_exclusion_config` sidecar verbatim across the
    // bincode / JSON round-trip so callers (impact / api_impact /
    // route_map / shape_check / edges) see the same policy
    // configuration the warmer persisted.
    let config = RouteExclusionConfig {
        health_path_globs: vec!["/_internal/health".to_string()],
        param_only_paths: false,
        min_confidence_for_consumer_edge: 0.75,
        excluded_frameworks: vec!["actix".to_string()],
    };

    let artifact = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![],
        edges: vec![],
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: vec![],
        route_exclusion_config: config.clone(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    };
    let graph = RepoDependencyGraph::from_artifact(&artifact);
    assert_eq!(graph.route_exclusion_config(), &config);

    // `set_route_exclusion_config` lets tests (and the warmer)
    // override the policy without rebuilding the graph.
    let override_cfg = RouteExclusionConfig {
        min_confidence_for_consumer_edge: 0.10,
        ..Default::default()
    };
    let mut graph = graph;
    graph.set_route_exclusion_config(override_cfg.clone());
    assert_eq!(graph.route_exclusion_config(), &override_cfg);
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

#[test]
fn bincode_v10_artifact_without_route_metadata_deserializes_with_none() {
    #[derive(Serialize)]
    struct V10RepoGraphNodeWithoutRouteMetadata {
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
        workspace: Option<String>,
    }

    #[derive(Serialize)]
    struct V10RepoGraphArtifactWithoutRouteMetadata {
        version: u32,
        nodes: Vec<V10RepoGraphNodeWithoutRouteMetadata>,
        edges: Vec<RepoGraphArtifactEdge>,
        symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
        communities: Vec<crate::communities::Community>,
        processes: Vec<RepoGraphArtifactProcess>,
    }

    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    assert!(
        artifact
            .nodes
            .iter()
            .all(|node| node.kind != RepoGraphNodeKind::Route
                && node.kind != RepoGraphNodeKind::Tool)
    );
    assert!(artifact.edges.iter().all(|edge| !matches!(
        edge.kind,
        RepoGraphEdgeKind::HandlesRoute | RepoGraphEdgeKind::Fetches
    )));

    let old_nodes = artifact
        .nodes
        .iter()
        .map(|node| V10RepoGraphNodeWithoutRouteMetadata {
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
            workspace: node.workspace.clone(),
        })
        .collect();
    let old_artifact = V10RepoGraphArtifactWithoutRouteMetadata {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: old_nodes,
        edges: artifact.edges.clone(),
        symbol_ranges: artifact.symbol_ranges.clone(),
        communities: artifact.communities.clone(),
        processes: artifact.processes.clone(),
    };

    let encoded = bincode::serialize(&old_artifact)
        .expect("serialize old v10 bincode without route metadata");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize old v10 bincode without route metadata");

    assert_eq!(decoded.version, REPO_GRAPH_ARTIFACT_VERSION);
    assert!(
        decoded
            .nodes
            .iter()
            .all(|node| node.route_framework.is_none())
    );
    assert!(
        decoded
            .nodes
            .iter()
            .all(|node| node.route_handler_symbol.is_none())
    );
    assert!(decoded.nodes.iter().any(|node| {
        node.kind == RepoGraphNodeKind::File && node.workspace.as_deref() == Some("root")
    }));

    let restored = RepoDependencyGraph::from_artifact(&decoded);
    assert_eq!(restored.node_count(), graph.node_count());
    assert_eq!(restored.edge_count(), graph.edge_count());
    let app_file = restored.file_node("src/app.rs").expect("app file");
    assert_eq!(restored.node(app_file).workspace.as_deref(), Some("root"));
    assert!(
        restored
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .is_some()
    );
}

#[test]
fn route_exclusion_config_default_round_trips_json() {
    let config = RouteExclusionConfig::default();
    assert_eq!(
        config.health_path_globs,
        vec![
            "/health", "/healthz", "/ping", "/readyz", "/livez", "/metrics"
        ]
    );
    assert!(config.param_only_paths);
    assert_eq!(config.min_confidence_for_consumer_edge, 0.5);
    assert!(config.excluded_frameworks.is_empty());

    let json = serde_json::to_string(&config).expect("serialize config");
    let restored: RouteExclusionConfig = serde_json::from_str(&json).expect("deserialize config");
    assert_eq!(restored, config);
}

#[test]
fn route_exclusion_config_persists_through_graph_artifact_sidecar() {
    let mut graph = RepoDependencyGraph::build(&[fixture_index()]);
    let config = RouteExclusionConfig {
        health_path_globs: vec!["/status*".to_string()],
        param_only_paths: false,
        min_confidence_for_consumer_edge: 0.8,
        excluded_frameworks: vec!["axum".to_string()],
    };
    graph.set_route_exclusion_config(config.clone());

    let artifact = graph.to_artifact();
    assert_eq!(artifact.route_exclusion_config, config);

    let encoded = bincode::serialize(&artifact).expect("serialize artifact");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded).expect("deserialize artifact");
    let restored = RepoDependencyGraph::from_artifact(&decoded);

    assert_eq!(decoded.route_exclusion_config, config);
    assert_eq!(restored.route_exclusion_config(), &config);
}

#[test]
fn layout_positions_persist_through_graph_artifact_sidecar() {
    let mut graph = RepoDependencyGraph::build(&[fixture_index()]);
    let layout_positions = crate::layout::derive_layout_positions(&graph);
    graph.set_layout_positions(layout_positions.clone());

    let artifact = graph.to_artifact();
    assert_eq!(artifact.layout_positions, layout_positions);

    let encoded = bincode::serialize(&artifact).expect("serialize artifact with layout");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize artifact with layout");
    let restored = RepoDependencyGraph::from_artifact(&decoded);

    assert_eq!(decoded.layout_positions, layout_positions);
    assert_eq!(restored.layout_positions(), &layout_positions);
}

#[test]
fn current_bincode_artifact_without_layout_positions_backfills_on_load() {
    #[derive(Serialize)]
    struct V10RepoGraphArtifactWithoutLayoutPositions {
        version: u32,
        nodes: Vec<RepoGraphNode>,
        edges: Vec<RepoGraphArtifactEdge>,
        symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
        communities: Vec<crate::communities::Community>,
        processes: Vec<RepoGraphArtifactProcess>,
        route_exclusion_config: RouteExclusionConfig,
    }

    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    let old_artifact = V10RepoGraphArtifactWithoutLayoutPositions {
        version: artifact.version,
        nodes: artifact.nodes.clone(),
        edges: artifact.edges.clone(),
        symbol_ranges: artifact.symbol_ranges.clone(),
        communities: artifact.communities.clone(),
        processes: artifact.processes.clone(),
        route_exclusion_config: artifact.route_exclusion_config.clone(),
    };

    let encoded =
        bincode::serialize(&old_artifact).expect("serialize artifact without layout positions");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize artifact without layout positions");
    assert!(decoded.layout_positions.is_empty());

    let restored = RepoDependencyGraph::from_artifact(&decoded);
    assert_eq!(restored.layout_positions().len(), restored.node_count());
    for node in restored.graph().node_weights() {
        let position = restored
            .layout_position_by_uid(&node.stable_uid())
            .expect("legacy artifact layout position backfilled");
        assert!(position.x.is_finite());
        assert!(position.y.is_finite());
    }
}

#[test]
fn current_bincode_artifact_without_galaxy_layout_loads_empty() {
    // Proposal lmkv: blobs written before the galaxy sidecar end at
    // `layout_positions`. They must still deserialize; galaxy fields hydrate
    // empty so a `code_graph snapshot` omits galaxy coordinates and the UI
    // falls back to its worker layout.
    #[derive(Serialize)]
    struct V10RepoGraphArtifactWithoutGalaxyLayout {
        version: u32,
        nodes: Vec<RepoGraphNode>,
        edges: Vec<RepoGraphArtifactEdge>,
        symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
        communities: Vec<crate::communities::Community>,
        processes: Vec<RepoGraphArtifactProcess>,
        route_exclusion_config: RouteExclusionConfig,
        layout_positions: BTreeMap<String, crate::layout::GraphLayoutPosition>,
    }

    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    let old_artifact = V10RepoGraphArtifactWithoutGalaxyLayout {
        version: artifact.version,
        nodes: artifact.nodes.clone(),
        edges: artifact.edges.clone(),
        symbol_ranges: artifact.symbol_ranges.clone(),
        communities: artifact.communities.clone(),
        processes: artifact.processes.clone(),
        route_exclusion_config: artifact.route_exclusion_config.clone(),
        layout_positions: artifact.layout_positions.clone(),
    };

    let encoded =
        bincode::serialize(&old_artifact).expect("serialize artifact without galaxy layout");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize artifact without galaxy layout");
    // 2D layout survives; galaxy sidecar is empty (no forced recompute on load).
    assert_eq!(decoded.layout_positions, artifact.layout_positions);
    assert!(decoded.galaxy_positions.is_empty());
    assert!(decoded.galaxy_degrees.is_empty());

    let restored = RepoDependencyGraph::from_artifact(&decoded);
    assert!(restored.galaxy_positions().is_empty());
}

#[test]
fn current_bincode_artifact_without_route_exclusion_config_loads_default() {
    #[derive(Serialize)]
    struct V10RepoGraphArtifactWithoutRouteExclusionConfig {
        version: u32,
        nodes: Vec<RepoGraphNode>,
        edges: Vec<RepoGraphArtifactEdge>,
        symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
        communities: Vec<crate::communities::Community>,
        processes: Vec<RepoGraphArtifactProcess>,
    }

    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    let old_artifact = V10RepoGraphArtifactWithoutRouteExclusionConfig {
        version: artifact.version,
        nodes: artifact.nodes.clone(),
        edges: artifact.edges.clone(),
        symbol_ranges: artifact.symbol_ranges.clone(),
        communities: artifact.communities.clone(),
        processes: artifact.processes.clone(),
    };

    let encoded = bincode::serialize(&old_artifact)
        .expect("serialize artifact without route exclusion config");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize artifact without route exclusion config");

    assert_eq!(
        decoded.route_exclusion_config,
        RouteExclusionConfig::default()
    );
    assert_eq!(
        RepoDependencyGraph::from_artifact(&decoded).node_count(),
        graph.node_count()
    );
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
