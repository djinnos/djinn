//! Free functions extracted from [`super::query`] to keep the main
//! query module under the repository's file-size guard (1500 lines /
//! 51200 bytes).  All items here are `pub(super)` so they remain
//! accessible to sibling modules within `graph_ops` without leaking
//! to the rest of the crate.

use djinn_control_plane::bridge::{CrateEdgeEntry, CrateGraphResponse, CrateNodeEntry, ProjectCtx};
use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
use djinn_graph::repo_graph::RepoDependencyGraph;
use petgraph::visit::EdgeRef;

use super::format_node_key;
#[cfg(test)]
use super::resolve_node_or_err;

pub(super) async fn crate_graph_from_warmed_cache(
    ctx: &ProjectCtx,
) -> Result<CrateGraphResponse, String> {
    let (_project_root, index_tree_path) =
        djinn_graph::canonical_graph::normalize_graph_query_paths(&ctx.clone_path);

    let (graph, crate_map) = {
        let cache = djinn_graph::canonical_graph::GRAPH_CACHE.read().await;
        let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.project_path == index_tree_path)
        else {
            return Ok(CrateGraphResponse {
                crates: Vec::new(),
                edges: Vec::new(),
                message: Some("Graph not warmed for this workspace.".to_string()),
            });
        };
        (cached.graph.clone(), cached.crate_map.clone())
    };

    if crate_map.is_empty() {
        return Ok(CrateGraphResponse {
            crates: Vec::new(),
            edges: Vec::new(),
            message: Some(
                "No crate mapping found — not a Rust workspace or workspace not yet warmed."
                    .to_string(),
            ),
        });
    }

    let crate_graph = djinn_graph::repo_graph::build_crate_graph(&graph, crate_map.as_ref());
    Ok(CrateGraphResponse {
        crates: crate_graph
            .crates
            .into_iter()
            .map(|node| CrateNodeEntry {
                name: node.name,
                manifest_path: node.manifest_path.to_string_lossy().into_owned(),
                loc: node.loc,
                node_count: node.node_count,
                fan_in: node.fan_in,
                fan_out: node.fan_out,
                inbound_weight: node.inbound_weight,
                outbound_weight: node.outbound_weight,
            })
            .collect(),
        edges: crate_graph
            .edges
            .into_iter()
            .map(|edge| CrateEdgeEntry {
                source: edge.source,
                target: edge.target,
                weight: edge.weight,
                edge_count: edge.edge_count,
            })
            .collect(),
        message: None,
    })
}

/// Collect the implementor symbols for the trait/interface node at
/// `target`. Extracted from [`super::query::RepoGraphBridge::implementations`]
/// so the same predicate (and exclusion filter) can be exercised in unit
/// tests without spinning up an `AppState`.
///
/// Walks every incoming `Implements` edge to `target`, drops external
/// implementors and anything filtered by `exclusions`, and returns the
/// remaining implementor SCIP symbol strings in graph order.
pub(super) fn collect_implementations(
    graph: &RepoDependencyGraph,
    target: petgraph::graph::NodeIndex,
    exclusions: &GraphExclusions,
) -> Vec<String> {
    use djinn_graph::repo_graph::RepoGraphEdgeKind;
    let mut impls = Vec::new();
    for edge in graph
        .graph()
        .edges_directed(target, petgraph::Direction::Incoming)
    {
        if edge.weight().kind != RepoGraphEdgeKind::Implements {
            continue;
        }
        let source_node = graph.node(edge.source());
        if source_node.is_external {
            continue;
        }
        let Some(sym) = &source_node.symbol else {
            continue;
        };
        let src_key = format_node_key(&source_node.id);
        let src_file = source_node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string());
        if exclusions.excludes(&src_key, src_file.as_deref(), &source_node.display_name) {
            continue;
        }
        impls.push(sym.clone());
    }
    impls
}

#[cfg(test)]
pub(super) mod test_helpers {
    //! Re-export the inner `implementations` algorithm and the shared
    //! resolver shim so the `graph_ops::tests` module can drive both
    //! the bare-SCIP and `symbol:<scip>`-prefixed call shapes against
    //! a fixture graph without standing up an `AppState`. Mirrors the
    //! `flow::test_helpers` / `routes::test_helpers` pattern used by
    //! the rest of the bridge op module.
    use super::*;
    use djinn_graph::repo_graph::RepoDependencyGraph;

    /// Resolve `key` against `graph` via the shared resolver and
    /// return the same implementor list the bridge op would return.
    /// `key` is forwarded verbatim — passing a bare SCIP symbol and
    /// the canonical `symbol:<scip>` form must produce identical
    /// results.
    pub(crate) fn implementations_for_graph(
        graph: &RepoDependencyGraph,
        key: &str,
    ) -> Result<Vec<String>, String> {
        let node_index = resolve_node_or_err(graph, key)?;
        Ok(collect_implementations(
            graph,
            node_index,
            &GraphExclusions::empty(),
        ))
    }
}
