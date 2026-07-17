// Tests for `RepoDependencyGraph::salvage_workspace_from_artifact` — the
// last-good splice used by the warm pipeline when a workspace's indexer
// failed or timed out.

use std::collections::BTreeMap;

use super::*;
use crate::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphArtifactSymbolRange, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    RouteExclusionConfig,
};

fn node(key: RepoNodeKey, kind: RepoGraphNodeKind, name: &str, workspace: &str) -> RepoGraphNode {
    let file_path = match &key {
        RepoNodeKey::File(path) => Some(path.clone()),
        _ => None,
    };
    let symbol = match &key {
        RepoNodeKey::Symbol(symbol) => Some(symbol.clone()),
        _ => None,
    };
    RepoGraphNode {
        id: key,
        kind,
        display_name: name.to_string(),
        language: Some("rust".to_string()),
        file_path,
        symbol,
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: Vec::new(),
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some(workspace.to_string()),
        route_framework: None,
        route_handler_symbol: None,
    }
}

fn edge(source: usize, target: usize, kind: RepoGraphEdgeKind) -> RepoGraphArtifactEdge {
    RepoGraphArtifactEdge {
        source,
        target,
        kind,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    }
}

fn artifact(nodes: Vec<RepoGraphNode>, edges: Vec<RepoGraphArtifactEdge>) -> RepoGraphArtifact {
    RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: vec![],
        route_exclusion_config: RouteExclusionConfig::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    }
}

/// The previous graph used across the tests below: a two-workspace project
/// where `server` has a file + a symbol and `ui` has one file, with an
/// intra-`server` edge, a cross-workspace edge, and a co-change row in the
/// shared edges vec.
fn previous_artifact() -> RepoGraphArtifact {
    let nodes = vec![
        // 0: server file
        node(
            RepoNodeKey::File(PathBuf::from("server/src/lib.rs")),
            RepoGraphNodeKind::File,
            "lib.rs",
            "server",
        ),
        // 1: server symbol
        node(
            RepoNodeKey::Symbol("scip-rust pkg server/src/lib.rs `handler`().".to_string()),
            RepoGraphNodeKind::Symbol,
            "handler",
            "server",
        ),
        // 2: ui file
        node(
            RepoNodeKey::File(PathBuf::from("ui/src/app.ts")),
            RepoGraphNodeKind::File,
            "app.ts",
            "ui",
        ),
    ];
    let mut artifact = artifact(
        nodes,
        vec![
            // intra-server: file contains symbol
            edge(0, 1, RepoGraphEdgeKind::ContainsDefinition),
            // cross-workspace: server symbol references the ui file
            edge(1, 2, RepoGraphEdgeKind::SymbolReference),
            // co-change sidecar row — must NOT be salvaged into the petgraph
            edge(0, 2, RepoGraphEdgeKind::CoChangedWith),
        ],
    );
    artifact.symbol_ranges.insert(
        PathBuf::from("server/src/lib.rs"),
        vec![RepoGraphArtifactSymbolRange {
            start_line: 3,
            end_line: 9,
            node: 1,
        }],
    );
    artifact
}

/// A fresh build where only `ui` indexed (the `server` indexer timed out):
/// exactly the previous artifact's ui file, nothing else.
fn fresh_ui_only_graph() -> RepoDependencyGraph {
    let previous = previous_artifact();
    let ui_only = artifact(vec![previous.nodes[2].clone()], vec![]);
    RepoDependencyGraph::from_artifact(&ui_only)
}

#[test]
fn salvage_splices_workspace_nodes_edges_and_ranges() {
    let previous = previous_artifact();
    let mut graph = fresh_ui_only_graph();
    assert_eq!(graph.node_count(), 1);

    let stats = graph.salvage_workspace_from_artifact(&previous, "server");

    assert_eq!(stats.nodes_added, 2, "server file + symbol spliced in");
    // Contains (intra-server) + SymbolReference (cross-workspace, resolved to
    // the fresh ui node by key). The CoChangedWith row stays out of the
    // petgraph.
    assert_eq!(stats.edges_added, 2);
    assert_eq!(graph.node_count(), 3);
    assert_eq!(graph.edge_count(), 2);
    assert!(graph.cochange_edges().is_empty());

    // The cross-workspace edge reattached to the FRESH ui node.
    let ui = graph.file_node("ui/src/app.ts").expect("fresh ui node");
    let symbol = graph
        .symbol_node("scip-rust pkg server/src/lib.rs `handler`().")
        .expect("salvaged server symbol");
    assert!(
        graph
            .graph()
            .edges_connecting(symbol, ui)
            .any(|e| e.weight().kind == RepoGraphEdgeKind::SymbolReference),
        "cross-workspace edge must resolve to the fresh ui node"
    );

    // Salvaged nodes are searchable (name index rebuilt) and their
    // enclosing-range sidecar came along for `symbols_at`.
    assert!(
        graph
            .search_by_name("handler", None, 10)
            .iter()
            .any(|hit| hit.node_index == symbol)
    );
    assert_eq!(
        graph.symbols_enclosing(Path::new("server/src/lib.rs"), 4, 5),
        vec![symbol]
    );
}

#[test]
fn salvage_skips_keys_that_already_exist_in_the_fresh_graph() {
    let previous = previous_artifact();
    // Fresh build where BOTH workspaces indexed — salvage must be a no-op
    // even if asked (e.g. one indexer of a polyglot workspace failed while
    // another covered it).
    let mut graph = RepoDependencyGraph::from_artifact(&artifact(
        previous.nodes.clone(),
        vec![edge(0, 1, RepoGraphEdgeKind::ContainsDefinition)],
    ));
    let nodes_before = graph.node_count();
    let edges_before = graph.edge_count();

    let stats = graph.salvage_workspace_from_artifact(&previous, "server");

    assert_eq!(stats, WorkspaceSalvageStats::default());
    assert_eq!(graph.node_count(), nodes_before);
    assert_eq!(graph.edge_count(), edges_before, "no duplicate edges");
}

#[test]
fn salvage_of_unknown_workspace_is_a_no_op() {
    let previous = previous_artifact();
    let mut graph = fresh_ui_only_graph();

    let stats = graph.salvage_workspace_from_artifact(&previous, "does-not-exist");

    assert_eq!(stats, WorkspaceSalvageStats::default());
    assert_eq!(graph.node_count(), 1);
    assert_eq!(graph.edge_count(), 0);
}

#[test]
fn salvaged_graph_round_trips_through_the_artifact() {
    let previous = previous_artifact();
    let mut graph = fresh_ui_only_graph();
    graph.salvage_workspace_from_artifact(&previous, "server");

    let blob = bincode::serialize(&graph.to_artifact()).expect("serialize salvaged graph");
    let restored = RepoDependencyGraph::from_artifact(
        &deserialize_repo_graph_artifact_bincode(&blob).expect("deserialize salvaged graph"),
    );

    assert_eq!(restored.node_count(), 3);
    assert_eq!(restored.edge_count(), 2);
    // Workspace tags survive, so a FUTURE salvage can re-splice from this
    // blob if the workspace fails again.
    assert_eq!(
        restored
            .graph()
            .node_weights()
            .filter(|node| node.workspace.as_deref() == Some("server"))
            .count(),
        2
    );
}
