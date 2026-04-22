use std::collections::BTreeMap;

use djinn_control_plane::bridge::{FileGroupEntry, GraphNeighbor, ImpactEntry};

use djinn_graph::repo_graph::{RepoDependencyGraph, RepoGraphNode, RepoNodeKey};

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
