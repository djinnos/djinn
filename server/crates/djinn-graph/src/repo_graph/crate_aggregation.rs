use std::collections::BTreeMap;
use std::path::PathBuf;

use petgraph::visit::EdgeRef;

use crate::repo_graph::{CrateEdge, CrateGraph, CrateNode, RepoDependencyGraph, RepoGraphNodeKind};

/// Build a `CrateGraph` from a `RepoDependencyGraph` and a crate-of-file mapping.
///
/// Walks all nodes in the graph. For file nodes, looks up the crate via
/// `crate_map` prefix match (longest prefix wins). For symbol nodes, uses the
/// symbol's `file_path` to resolve its crate. Nodes outside any workspace
/// member are grouped under a synthetic `"<external>"` crate.
///
/// Computes per-crate rollups:
/// - `node_count`: number of file + symbol nodes in the crate
/// - `loc`: for file nodes, sum of line counts (if available) or estimated from
///   symbol ranges; can start with node_count as proxy
/// - `fan_in` / `fan_out`: count of cross-crate inbound / outbound edges
/// - `inbound_weight` / `outbound_weight`: sum of edge weights crossing the crate
///   boundary
///
/// Aggregates cross-crate edges: for each edge where source node's crate ≠
/// target node's crate, adds or accumulates a `CrateEdge` between those crates.
pub fn build_crate_graph(
    graph: &RepoDependencyGraph,
    crate_map: &BTreeMap<PathBuf, String>,
) -> CrateGraph {
    let mut crate_lookup: BTreeMap<usize, String> = BTreeMap::new();
    let mut node_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut locs: BTreeMap<String, usize> = BTreeMap::new();
    let mut fan_in: BTreeMap<String, usize> = BTreeMap::new();
    let mut fan_out: BTreeMap<String, usize> = BTreeMap::new();
    let mut inbound_weight: BTreeMap<String, f64> = BTreeMap::new();
    let mut outbound_weight: BTreeMap<String, f64> = BTreeMap::new();

    // Resolve each node's crate membership.
    for node_idx in graph.graph.node_indices() {
        let node = &graph.graph[node_idx];
        let crate_name = match node.kind {
            RepoGraphNodeKind::File => {
                if let crate::repo_graph::RepoNodeKey::File(ref path) = node.id {
                    resolve_crate(path, crate_map)
                } else {
                    "<external>".to_string()
                }
            }
            RepoGraphNodeKind::Symbol => {
                if let Some(ref fp) = node.file_path {
                    resolve_crate(fp, crate_map)
                } else {
                    "<external>".to_string()
                }
            }
            // Process, Table, Route, Tool nodes are synthetic and don't count
            // toward node_count / loc, but they still need a crate for edge
            // aggregation. Resolve via file_path when available.
            _ => {
                if let Some(ref fp) = node.file_path {
                    resolve_crate(fp, crate_map)
                } else {
                    "<external>".to_string()
                }
            }
        };

        crate_lookup.insert(node_idx.index(), crate_name);
    }

    // Count file and symbol nodes per crate, and estimate LOC.
    for node_idx in graph.graph.node_indices() {
        let node = &graph.graph[node_idx];
        let crate_name = crate_lookup
            .get(&node_idx.index())
            .cloned()
            .unwrap_or_else(|| "<external>".to_string());

        match node.kind {
            RepoGraphNodeKind::File | RepoGraphNodeKind::Symbol => {
                *node_counts.entry(crate_name.clone()).or_insert(0) += 1;

                // LOC estimation: for file nodes, use symbol range count as proxy
                // if available; for symbol nodes, contribute 1 (they represent a
                // line-range). Start with node_count as baseline and add range
                // contributions when available.
                let loc_estimate = match node.kind {
                    RepoGraphNodeKind::File => {
                        if let crate::repo_graph::RepoNodeKey::File(ref path) = node.id {
                            graph
                                .symbol_ranges
                                .get(path)
                                .map(|ranges| ranges.len())
                                .unwrap_or(1)
                        } else {
                            1
                        }
                    }
                    RepoGraphNodeKind::Symbol => 1,
                    _ => 0,
                };
                *locs.entry(crate_name).or_insert(0) += loc_estimate;
            }
            _ => {}
        }
    }

    // Aggregate cross-crate edges.
    let mut crate_edge_map: BTreeMap<(String, String), (f64, usize)> = BTreeMap::new();

    for edge_ref in graph.graph.edge_references() {
        let source_idx = edge_ref.source();
        let target_idx = edge_ref.target();
        let source_crate = crate_lookup
            .get(&source_idx.index())
            .cloned()
            .unwrap_or_else(|| "<external>".to_string());
        let target_crate = crate_lookup
            .get(&target_idx.index())
            .cloned()
            .unwrap_or_else(|| "<external>".to_string());

        if source_crate != target_crate {
            let weight = edge_ref.weight().weight;
            let key = (source_crate.clone(), target_crate.clone());
            let (total_weight, count) = crate_edge_map.entry(key).or_insert((0.0, 0));
            *total_weight += weight;
            *count += 1;

            // Update fan_out for source crate and fan_in for target crate.
            *fan_out.entry(source_crate.clone()).or_insert(0) += 1;
            *fan_in.entry(target_crate.clone()).or_insert(0) += 1;
            *outbound_weight.entry(source_crate).or_insert(0.0) += weight;
            *inbound_weight.entry(target_crate).or_insert(0.0) += weight;
        }
    }

    // Build CrateEdge list.
    let mut edges: Vec<CrateEdge> = crate_edge_map
        .into_iter()
        .map(|((source, target), (weight, edge_count))| CrateEdge {
            source,
            target,
            weight,
            edge_count,
        })
        .collect();
    edges.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));

    // Collect all crate names that appear (either from nodes or edges).
    let mut all_crates: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (path, name) in crate_map {
        all_crates.insert(name.clone(), path.clone());
    }
    // Ensure <external> and any edge-only crates are present.
    for edge in &edges {
        all_crates.entry(edge.source.clone()).or_default();
        all_crates.entry(edge.target.clone()).or_default();
    }
    for crate_name in node_counts.keys() {
        all_crates.entry(crate_name.clone()).or_default();
    }

    let mut crates: Vec<CrateNode> = all_crates
        .into_iter()
        .map(|(name, manifest_path)| CrateNode {
            name: name.clone(),
            manifest_path,
            loc: locs.get(&name).copied().unwrap_or(0),
            node_count: node_counts.get(&name).copied().unwrap_or(0),
            fan_in: fan_in.get(&name).copied().unwrap_or(0) as f64,
            fan_out: fan_out.get(&name).copied().unwrap_or(0) as f64,
            inbound_weight: inbound_weight.get(&name).copied().unwrap_or(0.0),
            outbound_weight: outbound_weight.get(&name).copied().unwrap_or(0.0),
        })
        .collect();
    crates.sort_by(|a, b| a.name.cmp(&b.name));

    CrateGraph { crates, edges }
}

/// Resolve a file path to its crate name using the longest matching prefix in
/// `crate_map`.
#[allow(dead_code)] // exercised via build_crate_graph in tests
fn resolve_crate(path: &std::path::Path, crate_map: &BTreeMap<PathBuf, String>) -> String {
    let mut best_match: Option<(&PathBuf, &String)> = None;
    for (prefix, crate_name) in crate_map {
        if path.starts_with(prefix) {
            match best_match {
                Some((prev_prefix, _))
                    if prefix.as_os_str().len() > prev_prefix.as_os_str().len() =>
                {
                    best_match = Some((prefix, crate_name));
                }
                None => {
                    best_match = Some((prefix, crate_name));
                }
                _ => {}
            }
        }
    }
    best_match
        .map(|(_, name)| name.clone())
        .unwrap_or_else(|| "<external>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_graph::{
        REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
        RepoGraphArtifactSymbolRange, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    };
    use std::path::PathBuf;

    fn build_test_graph_fixture() -> RepoDependencyGraph {
        // Two crates: "crate-a" and "crate-b"
        // crate-a: src/lib.rs with a symbol `helper`
        // crate-b: src/main.rs with a symbol `main` that references `helper`
        let helper_node = RepoGraphNode {
            id: RepoNodeKey::Symbol("scip-rust pkg crate-a/src/lib.rs `helper`().".to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: "helper".to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from("crate-a/src/lib.rs")),
            symbol: Some("scip-rust pkg crate-a/src/lib.rs `helper`().".to_string()),
            symbol_kind: Some(crate::scip_parser::ScipSymbolKind::Function),
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
        let main_node = RepoGraphNode {
            id: RepoNodeKey::Symbol("scip-rust pkg crate-b/src/main.rs `main`().".to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: "main".to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from("crate-b/src/main.rs")),
            symbol: Some("scip-rust pkg crate-b/src/main.rs `main`().".to_string()),
            symbol_kind: Some(crate::scip_parser::ScipSymbolKind::Function),
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
        let file_a = RepoGraphNode {
            id: RepoNodeKey::File(PathBuf::from("crate-a/src/lib.rs")),
            kind: RepoGraphNodeKind::File,
            display_name: "crate-a/src/lib.rs".to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from("crate-a/src/lib.rs")),
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
        let file_b = RepoGraphNode {
            id: RepoNodeKey::File(PathBuf::from("crate-b/src/main.rs")),
            kind: RepoGraphNodeKind::File,
            display_name: "crate-b/src/main.rs".to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from("crate-b/src/main.rs")),
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

        let artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![file_a, helper_node, file_b, main_node],
            edges: vec![
                // file_a contains helper
                RepoGraphArtifactEdge {
                    source: 0,
                    target: 1,
                    kind: crate::repo_graph::RepoGraphEdgeKind::ContainsDefinition,
                    weight: 1.0,
                    evidence_count: 1,
                    confidence: 1.0,
                    reason: None,
                    step: None,
                },
                // file_b contains main
                RepoGraphArtifactEdge {
                    source: 2,
                    target: 3,
                    kind: crate::repo_graph::RepoGraphEdgeKind::ContainsDefinition,
                    weight: 1.0,
                    evidence_count: 1,
                    confidence: 1.0,
                    reason: None,
                    step: None,
                },
                // main references helper (cross-crate edge)
                RepoGraphArtifactEdge {
                    source: 3,
                    target: 1,
                    kind: crate::repo_graph::RepoGraphEdgeKind::SymbolReference,
                    weight: 2.5,
                    evidence_count: 1,
                    confidence: 0.9,
                    reason: None,
                    step: None,
                },
            ],
            symbol_ranges: BTreeMap::from([
                (
                    PathBuf::from("crate-a/src/lib.rs"),
                    vec![RepoGraphArtifactSymbolRange {
                        start_line: 1,
                        end_line: 5,
                        node: 1,
                    }],
                ),
                (
                    PathBuf::from("crate-b/src/main.rs"),
                    vec![RepoGraphArtifactSymbolRange {
                        start_line: 1,
                        end_line: 10,
                        node: 3,
                    }],
                ),
            ]),
            communities: vec![],
            processes: vec![],
            route_exclusion_config: crate::repo_graph::RouteExclusionConfig::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        };

        RepoDependencyGraph::from_artifact(&artifact)
    }

    #[test]
    fn test_build_crate_graph_two_crates() {
        let graph = build_test_graph_fixture();
        let mut crate_map: BTreeMap<PathBuf, String> = BTreeMap::new();
        crate_map.insert(PathBuf::from("crate-a"), "crate-a".to_string());
        crate_map.insert(PathBuf::from("crate-b"), "crate-b".to_string());

        let crate_graph = build_crate_graph(&graph, &crate_map);

        // Should have 2 crates (crate-a and crate-b) — all nodes are mapped.
        assert_eq!(
            crate_graph.crates.len(),
            2,
            "expected 2 crates: crate-a, crate-b"
        );

        let crate_a = crate_graph
            .crates
            .iter()
            .find(|c| c.name == "crate-a")
            .expect("crate-a should exist");
        let crate_b = crate_graph
            .crates
            .iter()
            .find(|c| c.name == "crate-b")
            .expect("crate-b should exist");

        // crate-a: 1 file node + 1 symbol node = 2 nodes
        assert_eq!(crate_a.node_count, 2, "crate-a should have 2 nodes");
        // crate-b: 1 file node + 1 symbol node = 2 nodes
        assert_eq!(crate_b.node_count, 2, "crate-b should have 2 nodes");

        // LOC: file_a has 1 symbol range, file_b has 1 symbol range
        // crate-a: file range (1) + helper symbol (1) = 2
        assert_eq!(crate_a.loc, 2, "crate-a loc should be 2");
        // crate-b: file range (1) + main symbol (1) = 2
        assert_eq!(crate_b.loc, 2, "crate-b loc should be 2");

        // crate-a fan_in: 1 inbound edge from main (crate-b) -> helper (crate-a)
        assert_eq!(crate_a.fan_in, 1.0, "crate-a fan_in should be 1");
        // crate-a fan_out: 0 outbound cross-crate edges
        assert_eq!(crate_a.fan_out, 0.0, "crate-a fan_out should be 0");
        // crate-a inbound_weight: 2.5 from the SymbolReference edge
        assert_eq!(
            crate_a.inbound_weight, 2.5,
            "crate-a inbound_weight should be 2.5"
        );
        // crate-a outbound_weight: 0
        assert_eq!(
            crate_a.outbound_weight, 0.0,
            "crate-a outbound_weight should be 0"
        );

        // crate-b fan_in: 0 inbound cross-crate edges
        assert_eq!(crate_b.fan_in, 0.0, "crate-b fan_in should be 0");
        // crate-b fan_out: 1 outbound edge to crate-a
        assert_eq!(crate_b.fan_out, 1.0, "crate-b fan_out should be 1");
        // crate-b inbound_weight: 0
        assert_eq!(
            crate_b.inbound_weight, 0.0,
            "crate-b inbound_weight should be 0"
        );
        // crate-b outbound_weight: 2.5
        assert_eq!(
            crate_b.outbound_weight, 2.5,
            "crate-b outbound_weight should be 2.5"
        );

        // Cross-crate edges
        assert_eq!(crate_graph.edges.len(), 1, "expected 1 cross-crate edge");
        let edge = &crate_graph.edges[0];
        assert_eq!(edge.source, "crate-b", "edge source should be crate-b");
        assert_eq!(edge.target, "crate-a", "edge target should be crate-a");
        assert_eq!(edge.weight, 2.5, "edge weight should be 2.5");
        assert_eq!(edge.edge_count, 1, "edge count should be 1");
    }

    #[test]
    fn test_build_crate_graph_unmapped_nodes_go_external() {
        let graph = build_test_graph_fixture();
        // Only map crate-a, leave crate-b unmapped
        let mut crate_map: BTreeMap<PathBuf, String> = BTreeMap::new();
        crate_map.insert(PathBuf::from("crate-a"), "crate-a".to_string());

        let crate_graph = build_crate_graph(&graph, &crate_map);

        let external = crate_graph
            .crates
            .iter()
            .find(|c| c.name == "<external>")
            .expect("<external> crate should exist");
        let crate_a = crate_graph
            .crates
            .iter()
            .find(|c| c.name == "crate-a")
            .expect("crate-a should exist");

        // crate-b nodes should be in <external>
        assert_eq!(
            external.node_count, 2,
            "external should have 2 nodes (crate-b's file + symbol)"
        );
        assert_eq!(crate_a.node_count, 2, "crate-a should still have 2 nodes");

        // Cross-crate edge: crate-a -> <external> (because crate-b is now external)
        // Actually: main (external) -> helper (crate-a), so external -> crate-a
        assert_eq!(crate_graph.edges.len(), 1);
        let edge = &crate_graph.edges[0];
        assert_eq!(edge.source, "<external>", "source should be <external>");
        assert_eq!(edge.target, "crate-a", "target should be crate-a");
    }

    #[test]
    fn test_longest_prefix_wins() {
        let mut crate_map: BTreeMap<PathBuf, String> = BTreeMap::new();
        crate_map.insert(PathBuf::from("server"), "server".to_string());
        crate_map.insert(
            PathBuf::from("server/crates/djinn-graph"),
            "djinn-graph".to_string(),
        );

        let path = PathBuf::from("server/crates/djinn-graph/src/lib.rs");
        let resolved = resolve_crate(&path, &crate_map);
        assert_eq!(resolved, "djinn-graph", "longest prefix should win");

        let path2 = PathBuf::from("server/src/main.rs");
        let resolved2 = resolve_crate(&path2, &crate_map);
        assert_eq!(
            resolved2, "server",
            "shorter prefix should match when no longer one"
        );
    }
}
