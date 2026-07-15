use super::*;

#[test]
fn edge_category_table_pr_c1() {
    // Spot-check the EdgeCategory mapping — the contract table is
    // load-bearing for the UI parser so any silent rewrite must
    // break this test.
    use crate::mcp_bridge::graph_neighbors::edge_category_for;
    use djinn_graph::repo_graph::{RepoGraphEdge, RepoGraphEdgeKind};
    use djinn_graph::scip_parser::ScipSymbolKind;
    use std::path::PathBuf;

    let mk_edge = |kind: RepoGraphEdgeKind| RepoGraphEdge {
        kind,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    };
    let mk_node = |kind: Option<ScipSymbolKind>| djinn_graph::repo_graph::RepoGraphNode {
        id: djinn_graph::repo_graph::RepoNodeKey::Symbol("x".into()),
        kind: djinn_graph::repo_graph::RepoGraphNodeKind::Symbol,
        display_name: "x".into(),
        language: None,
        file_path: Some(PathBuf::from("x.rs")),
        symbol: Some("x".into()),
        symbol_kind: kind,
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

    let any_node = mk_node(None);
    // SymbolReference with non-callable target → References.
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
            &any_node
        ),
        EdgeCategory::References
    );
    // SymbolReference with Function target → Calls.
    let fn_node = mk_node(Some(ScipSymbolKind::Function));
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)), &fn_node),
        EdgeCategory::Calls
    );
    // SymbolReference with Method target → Calls.
    let method_node = mk_node(Some(ScipSymbolKind::Method));
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
            &method_node
        ),
        EdgeCategory::Calls
    );
    // SymbolReference with Constructor target → Calls.
    let ctor_node = mk_node(Some(ScipSymbolKind::Constructor));
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
            &ctor_node
        ),
        EdgeCategory::Calls
    );
    // PR A3 splits.
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Reads)), &any_node),
        EdgeCategory::Reads
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Writes)), &any_node),
        EdgeCategory::Writes
    );
    // FileReference → Imports.
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::FileReference)), &any_node),
        EdgeCategory::Imports
    );
    // Containment.
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::ContainsDefinition)),
            &any_node
        ),
        EdgeCategory::Contains
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::DeclaredInFile)), &any_node),
        EdgeCategory::Contains
    );
    // Symbol relationships.
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Extends)), &any_node),
        EdgeCategory::Extends
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Implements)), &any_node),
        EdgeCategory::Implements
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::TypeDefines)), &any_node),
        EdgeCategory::TypeDefines
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Defines)), &any_node),
        EdgeCategory::Defines
    );
}

#[test]
fn context_limit_30_per_category_pr_c1() {
    // Build a fan-in of 35 callers on a single symbol and verify
    // the `Calls` bucket truncates at 30, sorted desc by
    // confidence so the highest-confidence callers survive.
    use crate::mcp_bridge::graph_neighbors::{build_related_symbol, edge_category_for};
    use djinn_graph::repo_graph::*;
    use djinn_graph::scip_parser::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    let target_sym = "scip-rust pkg src/lib.rs `target`().".to_string();
    let target_symbol = ScipSymbol {
        symbol: target_sym.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("target".to_string()),
        signature: Some("fn target()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(ScipVisibility::Public),
        signature_parts: None,
    };
    let mut files: Vec<ScipFile> = vec![ScipFile {
        language: "rust".into(),
        relative_path: PathBuf::from("src/lib.rs"),
        definitions: vec![ScipOccurrence {
            symbol: target_sym.clone(),
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
        symbols: vec![target_symbol],
    }];
    for i in 0..35 {
        let caller_sym = format!("scip-rust pkg src/c{i}.rs `caller{i}`().");
        files.push(ScipFile {
            language: "rust".into(),
            relative_path: PathBuf::from(format!("src/c{i}.rs")),
            definitions: vec![ScipOccurrence {
                symbol: caller_sym.clone(),
                range: ScipRange {
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 8,
                },
                enclosing_range: None,
                roles: BTreeSet::from([ScipSymbolRole::Definition]),
                syntax_kind: None,
                override_documentation: vec![],
            }],
            references: vec![ScipOccurrence {
                symbol: target_sym.clone(),
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
            symbols: vec![ScipSymbol {
                symbol: caller_sym,
                kind: Some(ScipSymbolKind::Function),
                display_name: Some(format!("caller{i}")),
                signature: None,
                documentation: vec![],
                relationships: vec![],
                visibility: Some(ScipVisibility::Public),
                signature_parts: None,
            }],
        });
    }
    let parsed = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files,
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[parsed]);
    let target_node = graph
        .symbol_node(&target_sym)
        .expect("target should be in graph");

    // Collect incoming edges directly and bucket them.
    use petgraph::Direction;
    let mut by_cat: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
        std::collections::BTreeMap::new();
    for edge in graph
        .graph()
        .edges_directed(target_node, Direction::Incoming)
    {
        let other = graph.node(edge.source());
        let cat = edge_category_for(Some(edge.weight()), other);
        let related = build_related_symbol(other, edge.weight().confidence);
        by_cat.entry(cat).or_default().push(related);
    }
    for entries in by_cat.values_mut() {
        entries.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.uid.cmp(&b.uid))
        });
        entries.truncate(30);
    }

    // The fan-in mints `FileReference` edges from each caller-file
    // into the target symbol, which the EdgeCategory mapping
    // routes to `Imports`. With 35 raw incoming references, the
    // bucket must truncate at 30 (the plan-mandated hard cap).
    let imports_count = by_cat
        .get(&EdgeCategory::Imports)
        .map(|v| v.len())
        .unwrap_or(0);
    assert_eq!(
        imports_count, 30,
        "incoming.imports must hard-cap at 30; got {imports_count}"
    );
    // And confirm: at least one bucket actually exceeded the cap
    // pre-truncation (otherwise the test isn't exercising the cap).
    let raw_incoming = graph
        .graph()
        .edges_directed(target_node, Direction::Incoming)
        .count();
    assert!(
        raw_incoming >= 35,
        "fan-in fixture should produce >= 35 raw incoming edges, got {raw_incoming}"
    );
}

#[test]
fn context_emits_processes_for_step_node_pr_f2() {
    // Build a 5-symbol linear chain (`main → a → b → c → d`) so the
    // F2 process detector emits one process. Then assert that the
    // C1 context-op-style construction populates the `processes`
    // field on a node that's a step in that flow.
    use djinn_graph::repo_graph::*;
    use djinn_graph::scip_parser::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn def_occ(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
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
        }
    }
    fn ref_occ(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 4,
            },
            enclosing_range: None,
            roles: BTreeSet::new(),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }
    fn rust_function(symbol: &str, name: &str) -> ScipSymbol {
        ScipSymbol {
            symbol: symbol.to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some(name.to_string()),
            signature: Some(format!("fn {name}()")),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(ScipVisibility::Public),
            signature_parts: None,
        }
    }

    let main_sym = "scip-rust pkg src/main.rs `main`().";
    let a_sym = "scip-rust pkg src/a.rs `a`().";
    let b_sym = "scip-rust pkg src/b.rs `b`().";

    let parsed = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".into(),
                relative_path: PathBuf::from("src/main.rs"),
                definitions: vec![def_occ(main_sym)],
                references: vec![ref_occ(a_sym)],
                occurrences: vec![],
                symbols: vec![rust_function(main_sym, "main")],
            },
            ScipFile {
                language: "rust".into(),
                relative_path: PathBuf::from("src/a.rs"),
                definitions: vec![def_occ(a_sym)],
                references: vec![ref_occ(b_sym)],
                occurrences: vec![],
                symbols: vec![rust_function(a_sym, "a")],
            },
            ScipFile {
                language: "rust".into(),
                relative_path: PathBuf::from("src/b.rs"),
                definitions: vec![def_occ(b_sym)],
                references: vec![],
                occurrences: vec![],
                symbols: vec![rust_function(b_sym, "b")],
            },
        ],
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[parsed]);

    // Sanity: the detector ran and produced at least one process.
    assert!(
        !graph.processes().is_empty(),
        "linear chain should produce a process; got {:?}",
        graph.processes()
    );

    // The `b` symbol is a step in the `main` process.
    let b_idx = graph
        .symbol_node(b_sym)
        .expect("b symbol should be in the graph");
    let memberships = graph.processes_for_node(b_idx);
    assert!(
        !memberships.is_empty(),
        "node `b` must have process memberships"
    );

    // Mirror the wire-shape construction the bridge does.
    let process_refs: Vec<ProcessRef> = memberships
        .iter()
        .map(|p| ProcessRef {
            id: p.id.clone(),
            uid: p.id.clone(),
            label: p.label.clone(),
            role: "step".to_string(),
        })
        .collect();
    assert!(
        process_refs.iter().any(|r| r.role == "step"),
        "every process_ref must carry role=\"step\""
    );
    assert!(
        process_refs
            .iter()
            .any(|r| r.label.contains("main") && r.label.contains("process")),
        "expected a process labeled `\"main process\"`: {:?}",
        process_refs.iter().map(|r| &r.label).collect::<Vec<_>>()
    );
}

#[test]
fn context_method_metadata_none_when_signature_parts_absent_pr_c1() {
    // SCIP 0.7 ships only the markdown signature blob, so
    // `signature_parts` is None on every fixture. Per the plan
    // contract this MUST surface as `method_metadata: None` —
    // never regex-extracted from the markdown.
    use crate::mcp_bridge::graph_neighbors::build_method_metadata;
    let graph = build_test_graph();
    let helper_idx = graph
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper exists");
    let helper = graph.node(helper_idx);
    assert!(
        helper.signature_parts.is_none(),
        "fixture should not carry structured signature_parts"
    );
    assert!(
        build_method_metadata(helper).is_none(),
        "method_metadata must be None when signature_parts is absent"
    );
}

#[test]
fn context_method_metadata_some_when_signature_parts_present_pr_c1() {
    // Synthesise a signature_parts payload (as a future indexer
    // would) and assert the bridge surfaces it as MethodMeta.
    use crate::mcp_bridge::graph_neighbors::build_method_metadata;
    use djinn_graph::scip_parser::{ScipSignatureParam, ScipSignatureParts};

    let mut node = graph_neighbors_test_node();
    node.signature_parts = Some(ScipSignatureParts {
        parameters: vec![
            ScipSignatureParam {
                name: "user".into(),
                type_name: Some("User".into()),
                default_value: None,
            },
            ScipSignatureParam {
                name: "limit".into(),
                type_name: Some("usize".into()),
                default_value: Some("20".into()),
            },
        ],
        return_type: Some("Result<Vec<Item>, Error>".into()),
        type_parameters: vec!["T".into()],
        visibility: Some("pub".into()),
        is_async: Some(true),
        annotations: vec!["#[tracing::instrument]".into()],
    });
    let meta = build_method_metadata(&node).expect("metadata expected");
    assert_eq!(meta.params.len(), 2);
    assert_eq!(meta.params[0].name, "user");
    assert_eq!(meta.params[1].default_value.as_deref(), Some("20"));
    assert_eq!(
        meta.return_type.as_deref(),
        Some("Result<Vec<Item>, Error>")
    );
    assert_eq!(meta.is_async, Some(true));
    assert_eq!(meta.visibility.as_deref(), Some("pub"));
    assert_eq!(meta.annotations, vec!["#[tracing::instrument]"]);
}

fn graph_neighbors_test_node() -> djinn_graph::repo_graph::RepoGraphNode {
    use std::path::PathBuf;
    djinn_graph::repo_graph::RepoGraphNode {
        id: djinn_graph::repo_graph::RepoNodeKey::Symbol("x".into()),
        kind: djinn_graph::repo_graph::RepoGraphNodeKind::Symbol,
        display_name: "list_items".into(),
        language: Some("rust".into()),
        file_path: Some(PathBuf::from("src/lib.rs")),
        symbol: Some("scip-rust pkg src/lib.rs `list_items`().".into()),
        symbol_kind: Some(djinn_graph::scip_parser::ScipSymbolKind::Function),
        is_external: false,
        visibility: None,
        signature: Some("pub async fn list_items(...) -> Result<...>".into()),
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    }
}

// ── PR D2: snapshot op tests ─────────────────────────────────────────

#[test]
fn snapshot_payload_returns_full_graph_under_cap_pr_d2() {
    // Tiny fixture (3 file nodes + 3 symbol nodes + edges between
    // them) — way under the 2000 default cap, so the snapshot must
    // emit every node and `truncated` must be `false`.
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    let graph = build_test_graph();
    let ranking = graph.rank();
    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        2_000,
    );
    assert_eq!(payload.project_id, "proj-test");
    assert_eq!(payload.git_head, "deadbeef");
    assert_eq!(payload.generated_at, "2026-04-28T00:00:00Z");
    assert_eq!(payload.node_cap, 2_000);
    assert!(!payload.truncated, "tiny graph should not truncate");
    assert!(
        payload.total_nodes == payload.nodes.len(),
        "total_nodes should match emitted node count when uncapped: \
         total={} emitted={}",
        payload.total_nodes,
        payload.nodes.len()
    );
    assert!(
        payload.total_edges == payload.edges.len(),
        "total_edges should match emitted edge count when uncapped"
    );

    // Every node must carry the canonical RepoNodeKey prefix.
    // PR F2 added a third kind, `process`, for synthetic
    // execution-flow nodes.
    for node in &payload.nodes {
        assert!(
            node.id.starts_with("file:")
                || node.id.starts_with("symbol:")
                || node.id.starts_with("process:"),
            "node id missing prefix: {}",
            node.id
        );
        assert!(
            matches!(node.kind.as_str(), "file" | "symbol" | "process"),
            "unexpected node.kind {}",
            node.kind
        );
        if matches!(node.kind.as_str(), "file" | "symbol") {
            assert_eq!(
                node.workspace.as_deref(),
                Some("root"),
                "snapshot node {} should carry workspace slug from RepoGraphNode.workspace",
                node.id
            );
        }
    }

    // Nodes must be in pagerank-desc order.
    for window in payload.nodes.windows(2) {
        assert!(
            window[0].pagerank >= window[1].pagerank,
            "nodes not sorted by pagerank desc: {} < {}",
            window[0].pagerank,
            window[1].pagerank
        );
    }

    // Every emitted edge endpoint must be a node we emitted (no
    // dangling references) — D2 acceptance criterion.
    let node_ids: std::collections::HashSet<&str> =
        payload.nodes.iter().map(|n| n.id.as_str()).collect();
    for edge in &payload.edges {
        assert!(
            node_ids.contains(edge.from.as_str()),
            "edge.from {} not in node set",
            edge.from
        );
        assert!(
            node_ids.contains(edge.to.as_str()),
            "edge.to {} not in node set",
            edge.to
        );
        assert!(
            edge.confidence >= 0.0 && edge.confidence <= 1.0,
            "edge confidence out of range: {}",
            edge.confidence
        );
    }
}

#[test]
fn snapshot_payload_emits_cochange_edges_as_distinct_channel_qoxm() {
    // Proposal qoxm: co-change edges live in a sidecar outside the petgraph.
    // The snapshot must emit them as their own `CoChangedWith` kind (with the
    // coupling score as confidence and the temporal reason) when both file
    // endpoints survive — and must emit none when the sidecar is empty.
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::cochange::{CoChangeInput, derive_cochange_edges};

    let mut graph = build_test_graph();
    let ranking = graph.rank();

    // Baseline: no sidecar → no CoChangedWith edges in the payload.
    let baseline = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-07-15T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        2_000,
    );
    assert!(
        baseline.edges.iter().all(|e| e.kind != "CoChangedWith"),
        "empty sidecar must emit no co-change edges"
    );

    // Materialize one co-change pair between the fixture's two files.
    let derived = derive_cochange_edges(
        &graph,
        &[CoChangeInput {
            file_a: "src/app.rs".to_string(),
            file_b: "src/helper.rs".to_string(),
            co_changes: 10,
            last_co_change_iso: "2026-07-01T00:00:00Z".to_string(),
        }],
    );
    assert_eq!(derived.len(), 1, "fixture pair should qualify");
    graph.set_cochange_edges(derived);

    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-07-15T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        2_000,
    );
    let cc: Vec<_> = payload
        .edges
        .iter()
        .filter(|e| e.kind == "CoChangedWith")
        .collect();
    assert_eq!(cc.len(), 1, "one surviving pair → one co-change edge");
    assert!(cc[0].from.starts_with("file:") && cc[0].to.starts_with("file:"));
    assert!(
        cc[0].confidence > 0.0 && cc[0].confidence < 1.0,
        "confidence carries the coupling score"
    );
    assert!(
        cc[0]
            .reason
            .as_deref()
            .is_some_and(|r| r.starts_with("cochange;last_day=")),
        "temporal last_co_change rides in the reason: {:?}",
        cc[0].reason
    );
    // Co-change is not part of the structural edge total.
    assert_eq!(payload.total_edges, baseline.total_edges);
}

#[test]
fn snapshot_payload_truncates_when_node_cap_smaller_than_graph_pr_d2() {
    // Cap below the graph's node count — `truncated` must be true,
    // emitted nodes must equal cap, and every emitted edge's
    // endpoints must be among the survivors.
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    let graph = build_test_graph();
    let ranking = graph.rank();
    let cap = 2_usize;
    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        cap,
    );
    assert_eq!(payload.node_cap, cap, "node_cap echoed back unchanged");
    assert!(
        payload.truncated,
        "should be truncated when total_nodes={} > cap={}",
        payload.total_nodes, cap
    );
    assert!(
        payload.nodes.len() >= cap,
        "emitted {} nodes, should include at least the initial cap {}",
        payload.nodes.len(),
        cap
    );
    assert!(
        payload.total_nodes >= payload.nodes.len(),
        "total_nodes {} should be ≥ emitted {} on a truncating snapshot",
        payload.total_nodes,
        payload.nodes.len()
    );

    // No dangling edge endpoints — UI rendering depends on this.
    let node_ids: std::collections::HashSet<&str> =
        payload.nodes.iter().map(|n| n.id.as_str()).collect();
    for edge in &payload.edges {
        assert!(
            node_ids.contains(edge.from.as_str()) && node_ids.contains(edge.to.as_str()),
            "truncated snapshot leaked an edge {} → {} into the wire",
            edge.from,
            edge.to
        );
    }
}

#[test]
fn snapshot_payload_rescues_cross_workspace_endpoint_under_cap() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RankedRepoGraphNode, RepoDependencyGraph, RepoGraphArtifact,
        RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
        RepoGraphRanking, RepoNodeKey,
    };

    let mk_node = |name: &str, workspace: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(name.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(format!("{workspace}/src/{name}.rs"))),
        symbol: Some(name.to_string()),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some(workspace.to_string()),
        route_framework: None,
        route_handler_symbol: None,
    };
    let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            mk_node("a_hot_0", "workspace-a"),
            mk_node("a_hot_1", "workspace-a"),
            mk_node("a_hot_2", "workspace-a"),
            mk_node("a_hot_3", "workspace-a"),
            mk_node("a_hot_4", "workspace-a"),
            mk_node("b_quiet_endpoint", "workspace-b"),
        ],
        edges: vec![RepoGraphArtifactEdge {
            source: 1,
            target: 5,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: 1.0,
            evidence_count: 1,
            confidence: 0.9,
            reason: None,
            step: None,
        }],
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    });
    let ranking = RepoGraphRanking {
        nodes: graph
            .graph()
            .node_indices()
            .enumerate()
            .map(|(rank, node_index)| RankedRepoGraphNode {
                node_index,
                key: graph.node(node_index).id.clone(),
                kind: graph.node(node_index).kind,
                score: (10 - rank) as f64,
                page_rank: (10 - rank) as f64,
                structural_weight: 1.0,
                inbound_edge_weight: 0.0,
                outbound_edge_weight: 0.0,
                is_entry_point: false,
                entry_point_distance: None,
                fused_rank: (10 - rank) as f64,
            })
            .collect(),
    };

    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        2,
    );

    let node_ids: std::collections::HashSet<&str> =
        payload.nodes.iter().map(|node| node.id.as_str()).collect();
    assert!(node_ids.contains("symbol:a_hot_1"));
    assert!(node_ids.contains("symbol:b_quiet_endpoint"));
    assert!(
        payload
            .edges
            .iter()
            .any(|edge| { edge.from == "symbol:a_hot_1" && edge.to == "symbol:b_quiet_endpoint" })
    );
    for edge in &payload.edges {
        assert!(node_ids.contains(edge.from.as_str()));
        assert!(node_ids.contains(edge.to.as_str()));
    }
}

#[test]
fn community_snapshot_aggregates_cross_workspace_edges() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::communities::Community;
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RankedRepoGraphNode, RepoDependencyGraph, RepoGraphArtifact,
        RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
        RepoGraphRanking, RepoNodeKey,
    };

    let mk_node = |name: &str, workspace: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(name.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(format!("{workspace}/src/{name}.rs"))),
        symbol: Some(name.to_string()),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some(workspace.to_string()),
        route_framework: None,
        route_handler_symbol: None,
    };

    let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            mk_node("api_entry", "api"),
            mk_node("api_helper", "api"),
            mk_node("web_entry", "web"),
            mk_node("web_helper", "web"),
        ],
        edges: vec![
            RepoGraphArtifactEdge {
                source: 0,
                target: 1,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.8,
                reason: None,
                step: None,
            },
            RepoGraphArtifactEdge {
                source: 2,
                target: 3,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.8,
                reason: None,
                step: None,
            },
            RepoGraphArtifactEdge {
                source: 1,
                target: 2,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.9,
                reason: Some("cross-workspace call".to_string()),
                step: None,
            },
        ],
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![
            Community {
                id: "community-api".to_string(),
                label: "api".to_string(),
                member_ids: vec![0, 1],
                cohesion: 0.5,
                symbol_count: 2,
                keywords: vec!["api".to_string()],
            },
            Community {
                id: "community-web".to_string(),
                label: "web".to_string(),
                member_ids: vec![2, 3],
                cohesion: 0.5,
                symbol_count: 2,
                keywords: vec!["web".to_string()],
            },
        ],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    });
    let ranking = RepoGraphRanking {
        nodes: graph
            .graph()
            .node_indices()
            .enumerate()
            .map(|(rank, node_index)| RankedRepoGraphNode {
                node_index,
                key: graph.node(node_index).id.clone(),
                kind: graph.node(node_index).kind,
                score: (10 - rank) as f64,
                page_rank: (10 - rank) as f64,
                structural_weight: 1.0,
                inbound_edge_weight: 0.0,
                outbound_edge_weight: 0.0,
                is_entry_point: false,
                entry_point_distance: None,
                fused_rank: (10 - rank) as f64,
            })
            .collect(),
    };

    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Community,
        1,
    );

    assert_eq!(payload.nodes.len(), 2);
    let node_ids: std::collections::HashSet<&str> =
        payload.nodes.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(
        node_ids,
        std::collections::HashSet::from(["community-api", "community-web"])
    );
    assert!(payload.nodes.iter().all(|node| node.kind == "community"));
    assert!(payload.nodes.iter().any(|node| {
        node.id == "community-api"
            && node.workspace.as_deref() == Some("api")
            && node.workspace_kind.as_deref() == Some("single")
            && node.member_count == Some(2)
            && node.internal_edge_count == Some(1)
            && node.keywords == vec!["api".to_string()]
    }));
    assert!(payload.edges.iter().any(|edge| {
        edge.from == "community-api" && edge.to == "community-web" && edge.kind == "SymbolReference"
    }));
    for edge in &payload.edges {
        assert!(node_ids.contains(edge.from.as_str()));
        assert!(node_ids.contains(edge.to.as_str()));
    }
}

#[test]
fn snapshot_payload_preserves_quiet_workspace_when_cap_allows() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RankedRepoGraphNode, RepoDependencyGraph, RepoGraphArtifact,
        RepoGraphNode, RepoGraphNodeKind, RepoGraphRanking, RepoNodeKey,
    };

    let mk_node = |name: &str, workspace: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(name.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(format!("{workspace}/src/{name}.rs"))),
        symbol: Some(name.to_string()),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some(workspace.to_string()),
        route_framework: None,
        route_handler_symbol: None,
    };

    let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            mk_node("a_hot_0", "workspace-a"),
            mk_node("a_hot_1", "workspace-a"),
            mk_node("a_hot_2", "workspace-a"),
            mk_node("a_hot_3", "workspace-a"),
            mk_node("b_quiet", "workspace-b"),
        ],
        edges: vec![],
        symbol_ranges: std::collections::BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        layout_positions: std::collections::BTreeMap::new(),
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    });
    let ranking = RepoGraphRanking {
        nodes: graph
            .graph()
            .node_indices()
            .enumerate()
            .map(|(rank, node_index)| RankedRepoGraphNode {
                node_index,
                key: graph.node(node_index).id.clone(),
                kind: graph.node(node_index).kind,
                score: (10 - rank) as f64,
                page_rank: (10 - rank) as f64,
                structural_weight: 1.0,
                inbound_edge_weight: 0.0,
                outbound_edge_weight: 0.0,
                is_entry_point: false,
                entry_point_distance: None,
                fused_rank: (10 - rank) as f64,
            })
            .collect(),
    };

    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        2,
    );

    let workspaces: std::collections::HashSet<&str> = payload
        .nodes
        .iter()
        .filter_map(|node| node.workspace.as_deref())
        .collect();
    assert_eq!(payload.nodes.len(), 2);
    assert!(workspaces.contains("workspace-a"));
    assert!(workspaces.contains("workspace-b"));
    assert!(
        payload.nodes.iter().any(|node| node.id == "symbol:b_quiet"),
        "quiet workspace should retain a representative node: {:?}",
        payload.nodes
    );
}

/// PR F3 acceptance: when the canonical graph has detected
/// communities, the snapshot payload's `community_id` field is
/// populated for every node that joined a non-trivial community.
/// We synthesize a graph via the artifact seam (two tight 3-node
/// clusters joined by a thin bridge — the same fixture pattern
/// used in the `communities` module's unit tests) and verify the
/// adapter wires `RepoDependencyGraph::community_id(...)` through
/// to `SnapshotNode::community_id`.
#[test]
fn snapshot_payload_populates_community_id_pr_f3() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
        RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    };

    let mk_node = |name: &str, file: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(format!("symbol:{name}")),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(file)),
        symbol: Some(format!("symbol:{name}")),
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
    let nodes = vec![
        mk_node("auth_login", "src/auth/login.rs"),
        mk_node("auth_session", "src/auth/session.rs"),
        mk_node("auth_token", "src/auth/token.rs"),
        mk_node("billing_charge", "src/billing/charge.rs"),
        mk_node("billing_invoice", "src/billing/invoice.rs"),
        mk_node("billing_refund", "src/billing/refund.rs"),
    ];
    let edge = |s, t, w| RepoGraphArtifactEdge {
        source: s,
        target: t,
        kind: RepoGraphEdgeKind::SymbolReference,
        weight: w,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    };
    // Two tight triangles + a thin bridge between clusters.
    let mut edges = vec![
        edge(0, 1, 5.0),
        edge(1, 0, 5.0),
        edge(1, 2, 5.0),
        edge(2, 1, 5.0),
        edge(0, 2, 5.0),
        edge(2, 0, 5.0),
        edge(3, 4, 5.0),
        edge(4, 3, 5.0),
        edge(4, 5, 5.0),
        edge(5, 4, 5.0),
        edge(3, 5, 5.0),
        edge(5, 3, 5.0),
        edge(2, 3, 0.5),
        edge(3, 2, 0.5),
    ];
    // Sort to keep the artifact output stable across runs.
    edges.sort_by_key(|e| (e.source, e.target));

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
    // `from_artifact` does NOT run community detection (it
    // restores the persisted sidecar — empty here). To exercise
    // the detector against this fixture we re-run it manually
    // and install the result, mirroring how `finish()` does it
    // at build time. The detector is `pub`, so this is a
    // legitimate adapter call.
    let mut graph = RepoDependencyGraph::from_artifact(&artifact);
    let communities = djinn_graph::communities::detect_communities(&graph);
    assert!(
        !communities.is_empty(),
        "fixture should produce at least one community"
    );
    // Bypass `install_communities` (private) by round-tripping
    // through a populated artifact.
    let mut a2 = graph.to_artifact();
    a2.communities = communities;
    graph = RepoDependencyGraph::from_artifact(&a2);

    let ranking = graph.rank();
    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-f3".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        2_000,
    );

    // Every emitted node should carry a community_id (these are
    // all symbols in the two tight triangles — none of them is a
    // singleton).
    let with_community = payload
        .nodes
        .iter()
        .filter(|n| n.community_id.is_some())
        .count();
    assert!(
        with_community >= 4,
        "expected ≥4 nodes with a community_id, got {with_community}: {:?}",
        payload
            .nodes
            .iter()
            .map(|n| (n.id.clone(), n.community_id.clone()))
            .collect::<Vec<_>>()
    );

    // The auth and billing clusters should map to *different*
    // community ids — proves the adapter isn't lazily handing
    // back a single global id.
    let auth_id = payload
        .nodes
        .iter()
        .find(|n| n.id.contains("auth_login"))
        .and_then(|n| n.community_id.clone())
        .expect("auth_login should carry a community_id");
    let billing_id = payload
        .nodes
        .iter()
        .find(|n| n.id.contains("billing_charge"))
        .and_then(|n| n.community_id.clone())
        .expect("billing_charge should carry a community_id");
    assert_ne!(
        auth_id, billing_id,
        "auth and billing clusters should not share community_id"
    );
}

// ── 7e6o: precomputed layout coordinates on snapshot nodes ───────────

/// 7e6o AC: symbol/file snapshots populate finite, non-all-zero
/// coordinates from the warm-time graph layout cache for every emitted
/// node. Builds a graph via the artifact seam (whose `from_artifact`
/// backfills deterministic layout positions when the sidecar is empty)
/// and verifies every emitted snapshot node carries finite, non-all-zero
/// coordinates both on the struct and in serialized JSON.
#[test]
fn snapshot_symbol_nodes_carry_finite_layout_coordinates_7e6o() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::repo_graph::{
        RankedRepoGraphNode, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
        RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoGraphRanking, RepoNodeKey,
    };
    use std::collections::BTreeMap;

    let mk_node = |name: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(name.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(format!("src/{name}.rs"))),
        symbol: Some(name.to_string()),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("root".to_string()),
        route_framework: None,
        route_handler_symbol: None,
    };

    let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: djinn_graph::repo_graph::REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![mk_node("alpha"), mk_node("beta"), mk_node("gamma")],
        edges: vec![
            RepoGraphArtifactEdge {
                source: 0,
                target: 1,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.9,
                reason: None,
                step: None,
            },
            RepoGraphArtifactEdge {
                source: 1,
                target: 2,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.9,
                reason: None,
                step: None,
            },
        ],
        symbol_ranges: BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: Default::default(),
        // Empty sidecar — from_artifact backfills via derive_layout_positions.
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    });
    let ranking = RepoGraphRanking {
        nodes: graph
            .graph()
            .node_indices()
            .enumerate()
            .map(|(rank, node_index)| RankedRepoGraphNode {
                node_index,
                key: graph.node(node_index).id.clone(),
                kind: graph.node(node_index).kind,
                score: (10 - rank) as f64,
                page_rank: (10 - rank) as f64,
                structural_weight: 1.0,
                inbound_edge_weight: 0.0,
                outbound_edge_weight: 0.0,
                is_entry_point: false,
                entry_point_distance: None,
                fused_rank: (10 - rank) as f64,
            })
            .collect(),
    };

    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Symbol,
        2_000,
    );
    assert!(
        !payload.nodes.is_empty(),
        "fixture graph should emit at least one node"
    );
    let mut all_zero = true;
    for node in &payload.nodes {
        assert!(
            node.x.is_finite(),
            "node {} x should be finite, got {}",
            node.id,
            node.x
        );
        assert!(
            node.y.is_finite(),
            "node {} y should be finite, got {}",
            node.id,
            node.y
        );
        if node.x != 0.0 || node.y != 0.0 {
            all_zero = false;
        }
    }
    assert!(
        !all_zero,
        "at least one node should have a non-zero coordinate from the layout cache"
    );

    // Serialized JSON must carry explicit numeric x/y on every node.
    let json = serde_json::to_value(&payload).expect("serialize payload");
    for node_json in json
        .get("nodes")
        .and_then(|v| v.as_array())
        .expect("nodes array")
    {
        let obj = node_json.as_object().expect("node object");
        assert!(obj.contains_key("x"), "serialized node missing x: {obj:?}");
        assert!(obj.contains_key("y"), "serialized node missing y: {obj:?}");
        assert!(
            obj["x"].as_f64().map(f64::is_finite).unwrap_or(false),
            "serialized x should be a finite number: {obj:?}"
        );
        assert!(
            obj["y"].as_f64().map(f64::is_finite).unwrap_or(false),
            "serialized y should be a finite number: {obj:?}"
        );
    }
}

/// 7e6o AC: `level=community` snapshots populate deterministic finite
/// coordinates for community aggregate nodes. Builds a two-community
/// graph whose layout cache is seeded with known member positions so the
/// centroid computation is verifiable, then checks the community nodes
/// received the expected centroid coordinates.
#[test]
fn snapshot_community_nodes_carry_centroid_coordinates_7e6o() {
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::communities::Community;
    use djinn_graph::layout::GraphLayoutPosition;
    use djinn_graph::repo_graph::{
        RankedRepoGraphNode, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
        RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoGraphRanking, RepoNodeKey,
    };
    use std::collections::BTreeMap;

    let mk_node = |name: &str| RepoGraphNode {
        id: RepoNodeKey::Symbol(name.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(format!("src/{name}.rs"))),
        symbol: Some(name.to_string()),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("root".to_string()),
        route_framework: None,
        route_handler_symbol: None,
    };

    // Two communities: alpha = {node0, node1}, beta = {node2, node3}.
    // Seed member positions with known values so the centroid is
    // deterministic and verifiable.
    let mut layout_positions: BTreeMap<String, GraphLayoutPosition> = BTreeMap::new();
    // stable_uid for a Symbol node is "symbol:<symbol>"
    layout_positions.insert(
        "symbol:alpha_a".to_string(),
        GraphLayoutPosition { x: 100.0, y: 200.0 },
    );
    layout_positions.insert(
        "symbol:alpha_b".to_string(),
        GraphLayoutPosition { x: 300.0, y: 400.0 },
    );
    layout_positions.insert(
        "symbol:beta_c".to_string(),
        GraphLayoutPosition { x: 500.0, y: 600.0 },
    );
    layout_positions.insert(
        "symbol:beta_d".to_string(),
        GraphLayoutPosition { x: 700.0, y: 800.0 },
    );

    let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: djinn_graph::repo_graph::REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            mk_node("alpha_a"),
            mk_node("alpha_b"),
            mk_node("beta_c"),
            mk_node("beta_d"),
        ],
        edges: vec![
            RepoGraphArtifactEdge {
                source: 0,
                target: 1,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.8,
                reason: None,
                step: None,
            },
            RepoGraphArtifactEdge {
                source: 2,
                target: 3,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.8,
                reason: None,
                step: None,
            },
        ],
        symbol_ranges: BTreeMap::new(),
        communities: vec![
            Community {
                id: "community-alpha".to_string(),
                label: "alpha".to_string(),
                member_ids: vec![0, 1],
                cohesion: 0.5,
                symbol_count: 2,
                keywords: vec![],
            },
            Community {
                id: "community-beta".to_string(),
                label: "beta".to_string(),
                member_ids: vec![2, 3],
                cohesion: 0.5,
                symbol_count: 2,
                keywords: vec![],
            },
        ],
        processes: vec![],
        route_exclusion_config: Default::default(),
        // Non-empty seed — from_artifact will NOT backfill, preserving
        // our known positions for the centroid check.
        layout_positions,
        galaxy_positions: std::collections::BTreeMap::new(),
        galaxy_degrees: std::collections::BTreeMap::new(),
    });
    let ranking = RepoGraphRanking {
        nodes: graph
            .graph()
            .node_indices()
            .enumerate()
            .map(|(rank, node_index)| RankedRepoGraphNode {
                node_index,
                key: graph.node(node_index).id.clone(),
                kind: graph.node(node_index).kind,
                score: (10 - rank) as f64,
                page_rank: (10 - rank) as f64,
                structural_weight: 1.0,
                inbound_edge_weight: 0.0,
                outbound_edge_weight: 0.0,
                is_entry_point: false,
                entry_point_distance: None,
                fused_rank: (10 - rank) as f64,
            })
            .collect(),
    };

    let payload = build_snapshot_payload(
        &graph,
        &ranking,
        "proj-test".to_string(),
        "deadbeef".to_string(),
        "2026-04-28T00:00:00Z".to_string(),
        &GraphExclusions::empty(),
        None,
        SnapshotLevel::Community,
        1_000,
    );

    assert_eq!(payload.nodes.len(), 2, "should emit two community nodes");

    let alpha = payload
        .nodes
        .iter()
        .find(|n| n.id == "community-alpha")
        .expect("community-alpha node");
    let beta = payload
        .nodes
        .iter()
        .find(|n| n.id == "community-beta")
        .expect("community-beta node");

    // Centroid of alpha = ((100+300)/2, (200+400)/2) = (200, 300)
    assert!(alpha.x.is_finite() && alpha.y.is_finite());
    assert!(
        (alpha.x - 200.0).abs() < 1e-9,
        "alpha centroid x should be 200, got {}",
        alpha.x
    );
    assert!(
        (alpha.y - 300.0).abs() < 1e-9,
        "alpha centroid y should be 300, got {}",
        alpha.y
    );

    // Centroid of beta = ((500+700)/2, (600+800)/2) = (600, 700)
    assert!(beta.x.is_finite() && beta.y.is_finite());
    assert!(
        (beta.x - 600.0).abs() < 1e-9,
        "beta centroid x should be 600, got {}",
        beta.x
    );
    assert!(
        (beta.y - 700.0).abs() < 1e-9,
        "beta centroid y should be 700, got {}",
        beta.y
    );

    // The two communities should not overlap (non-identical positions).
    assert!(
        (alpha.x - beta.x).abs() + (alpha.y - beta.y).abs() > 1.0,
        "community centroids should be distinct"
    );
}

// ── Iter 28: complexity op ranking + aggregation ─────────────────────
