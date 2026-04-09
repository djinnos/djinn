use std::collections::{BTreeMap, BTreeSet};

use djinn_mcp::bridge::{
    FileGroupEntry, GraphDiff, GraphDiffEdge, GraphDiffNode, GraphNeighbor, ImpactEntry,
};
use petgraph::visit::EdgeRef;

use crate::repo_graph::{RepoDependencyGraph, RepoGraphNode, RepoNodeKey};

pub(super) fn group_neighbors_by_file(
    neighbors: &[(&RepoGraphNode, GraphNeighbor)],
) -> Vec<FileGroupEntry> {
    let mut by_file: BTreeMap<String, FileGroupEntry> = BTreeMap::new();
    for (node, neighbor) in neighbors {
        let file_label = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| match &node.id {
                RepoNodeKey::File(p) => p.display().to_string(),
                RepoNodeKey::Symbol(s) => s.clone(),
            });
        let entry = by_file.entry(file_label.clone()).or_insert(FileGroupEntry {
            file: file_label,
            occurrence_count: 0,
            max_depth: 1,
            sample_keys: Vec::new(),
        });
        entry.occurrence_count += 1;
        if entry.sample_keys.len() < 5 {
            entry.sample_keys.push(neighbor.key.clone());
        }
    }
    by_file.into_values().collect()
}

pub(super) fn group_impact_by_file(
    graph: &RepoDependencyGraph,
    entries: &[(petgraph::graph::NodeIndex, ImpactEntry)],
) -> Vec<FileGroupEntry> {
    let mut by_file: BTreeMap<String, FileGroupEntry> = BTreeMap::new();
    for (idx, entry) in entries {
        let node = graph.node(*idx);
        let file_label = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| node.display_name.clone());
        let group = by_file.entry(file_label.clone()).or_insert(FileGroupEntry {
            file: file_label,
            occurrence_count: 0,
            max_depth: 0,
            sample_keys: Vec::new(),
        });
        group.occurrence_count += 1;
        if entry.depth > group.max_depth {
            group.max_depth = entry.depth;
        }
        if group.sample_keys.len() < 5 {
            group.sample_keys.push(entry.key.clone());
        }
    }
    by_file.into_values().collect()
}

pub(super) fn collect_diff_nodes(graph: &RepoDependencyGraph) -> Vec<GraphDiffNode> {
    graph
        .graph()
        .node_indices()
        .map(|idx| {
            let node = graph.node(idx);
            GraphDiffNode {
                key: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
            }
        })
        .collect()
}

pub(super) fn collect_diff_edges(graph: &RepoDependencyGraph) -> Vec<GraphDiffEdge> {
    graph
        .graph()
        .edge_references()
        .map(|edge| GraphDiffEdge {
            from: format_node_key(&graph.node(edge.source()).id),
            to: format_node_key(&graph.node(edge.target()).id),
            edge_kind: format!("{:?}", edge.weight().kind),
        })
        .collect()
}

pub(super) fn compute_graph_diff(
    previous: &RepoDependencyGraph,
    base_commit: String,
    current: &RepoDependencyGraph,
    head_commit: String,
) -> GraphDiff {
    fn node_keys(graph: &RepoDependencyGraph) -> BTreeSet<String> {
        graph
            .graph()
            .node_indices()
            .map(|idx| format_node_key(&graph.node(idx).id))
            .collect()
    }

    fn edge_keys(graph: &RepoDependencyGraph) -> BTreeSet<(String, String, String)> {
        graph
            .graph()
            .edge_references()
            .map(|edge| {
                (
                    format_node_key(&graph.node(edge.source()).id),
                    format_node_key(&graph.node(edge.target()).id),
                    format!("{:?}", edge.weight().kind),
                )
            })
            .collect()
    }

    let prev_nodes = node_keys(previous);
    let curr_nodes = node_keys(current);
    let prev_edges = edge_keys(previous);
    let curr_edges = edge_keys(current);

    let added_nodes: Vec<GraphDiffNode> = curr_nodes
        .difference(&prev_nodes)
        .map(|key| graph_diff_node_for_key(current, key))
        .collect();
    let removed_nodes: Vec<GraphDiffNode> = prev_nodes
        .difference(&curr_nodes)
        .map(|key| graph_diff_node_for_key(previous, key))
        .collect();
    let added_edges: Vec<GraphDiffEdge> = curr_edges
        .difference(&prev_edges)
        .map(|(from, to, edge_kind)| GraphDiffEdge {
            from: from.clone(),
            to: to.clone(),
            edge_kind: edge_kind.clone(),
        })
        .collect();
    let removed_edges: Vec<GraphDiffEdge> = prev_edges
        .difference(&curr_edges)
        .map(|(from, to, edge_kind)| GraphDiffEdge {
            from: from.clone(),
            to: to.clone(),
            edge_kind: edge_kind.clone(),
        })
        .collect();

    GraphDiff {
        base_commit: Some(base_commit),
        head_commit: Some(head_commit),
        added_nodes,
        removed_nodes,
        added_edges,
        removed_edges,
    }
}

fn graph_diff_node_for_key(graph: &RepoDependencyGraph, key: &str) -> GraphDiffNode {
    let display = graph
        .graph()
        .node_indices()
        .find(|idx| format_node_key(&graph.node(*idx).id) == key)
        .map(|idx| {
            let node = graph.node(idx);
            (
                node.display_name.clone(),
                format!("{:?}", node.kind).to_lowercase(),
            )
        })
        .unwrap_or_else(|| (key.to_string(), "unknown".to_string()));
    GraphDiffNode {
        key: key.to_string(),
        kind: display.1,
        display_name: display.0,
    }
}

pub(super) fn format_node_key(key: &RepoNodeKey) -> String {
    match key {
        RepoNodeKey::File(path) => format!("file:{}", path.display()),
        RepoNodeKey::Symbol(sym) => format!("symbol:{sym}"),
    }
}

pub(super) fn resolve_node(
    graph: &RepoDependencyGraph,
    key: &str,
) -> Result<petgraph::graph::NodeIndex, String> {
    let stripped = key.strip_prefix("file:").unwrap_or(key);
    if let Some(index) = graph.file_node(stripped) {
        return Ok(index);
    }

    let stripped = key.strip_prefix("symbol:").unwrap_or(key);
    if let Some(index) = graph.symbol_node(stripped) {
        return Ok(index);
    }

    Err(format!("node '{key}' not found in graph"))
}
