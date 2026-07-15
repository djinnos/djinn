use super::*;

/// The snapshot must cap the drawable-edge population so the wire payload
/// stays small enough to parse on cold load, while keeping containment
/// edges (the UI needs them to nest symbols) and reporting the full
/// post-exclusion edge count.
#[test]
fn snapshot_caps_drawable_edges_but_keeps_containment() {
    use crate::mcp_bridge::snapshot::SNAPSHOT_DRAWABLE_EDGE_CAP;
    use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
    use djinn_graph::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RankedRepoGraphNode, RepoDependencyGraph, RepoGraphArtifact,
        RepoGraphArtifactEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
        RepoGraphRanking, RepoNodeKey,
    };

    // Complete graph over N symbols in one workspace ⇒ N*(N-1)/2 drawable
    // (SymbolReference) edges, comfortably over the cap.
    let n = 160usize;
    let drawable_total = n * (n - 1) / 2;
    assert!(
        drawable_total > SNAPSHOT_DRAWABLE_EDGE_CAP,
        "fixture must exceed the cap to exercise it"
    );

    let mk = |i: usize| RepoGraphNode {
        id: RepoNodeKey::Symbol(format!("s{i}")),
        kind: RepoGraphNodeKind::Symbol,
        display_name: format!("s{i}"),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(format!("ws/src/s{i}.rs"))),
        symbol: Some(format!("s{i}")),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some("ws".to_string()),
        route_framework: None,
        route_handler_symbol: None,
    };
    let nodes: Vec<RepoGraphNode> = (0..n).map(mk).collect();
    let mut edges: Vec<RepoGraphArtifactEdge> = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            edges.push(RepoGraphArtifactEdge {
                source: i,
                target: j,
                kind: RepoGraphEdgeKind::SymbolReference,
                weight: 1.0,
                evidence_count: 1,
                confidence: 0.9,
                reason: None,
                step: None,
            });
        }
    }
    // A containment edge must survive the cap regardless.
    edges.push(RepoGraphArtifactEdge {
        source: 0,
        target: 1,
        kind: RepoGraphEdgeKind::ContainsDefinition,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.95,
        reason: None,
        step: None,
    });

    let graph = RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
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
    });
    let ranking = RepoGraphRanking {
        nodes: graph
            .graph()
            .node_indices()
            .map(|node_index| RankedRepoGraphNode {
                node_index,
                key: graph.node(node_index).id.clone(),
                kind: graph.node(node_index).kind,
                score: 1.0,
                page_rank: 1.0,
                structural_weight: 1.0,
                inbound_edge_weight: 0.0,
                outbound_edge_weight: 0.0,
                is_entry_point: false,
                entry_point_distance: None,
                fused_rank: 1.0,
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
        10_000,
    );

    let drawable_emitted = payload
        .edges
        .iter()
        .filter(|e| e.kind == "SymbolReference")
        .count();
    let containment_emitted = payload
        .edges
        .iter()
        .filter(|e| e.kind == "ContainsDefinition")
        .count();

    assert_eq!(
        drawable_emitted, SNAPSHOT_DRAWABLE_EDGE_CAP,
        "drawable edges capped to the ceiling, not shipped whole"
    );
    assert!(
        containment_emitted >= 1,
        "containment edge must survive the cap (nesting depends on it)"
    );
    assert!(
        payload.total_edges >= drawable_total,
        "total_edges reports the full post-exclusion count for the UI's \"N of M\""
    );
}
