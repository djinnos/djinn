//! `path` + `edges` — pairwise and tabular edge queries, split out of
//! `query.rs` to keep both under the Server Size Guard budget. Same
//! `impl RepoGraphBridge` split pattern as `snapshot.rs` / `insights.rs`.

use super::*;

impl RepoGraphBridge {
    /// Proposal qoxm note: `path` (like `impact`) deliberately has NO opt-in
    /// for `CoChangedWith` edges — a deviation from the "include via edge-kind
    /// filters" clause chosen on purpose. Threading co-change into a multi-hop
    /// shortest-path walk would let circumstantial commit history bridge
    /// structurally unconnected code and masquerade as a dependency chain.
    /// Single-hop consumption is served by `neighbors`
    /// (`kind_filter=co_changed_with`) and `edges`
    /// (`edge_kind=CoChangedWith`); `impact`'s exclusion lives in
    /// `shared::edge_propagates`. The sidecar never enters the petgraph this
    /// walk runs on, so exclusion here is structural, not a filter.
    pub(super) async fn path(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        from: &str,
        to: &str,
        max_depth: Option<usize>,
    ) -> Result<Option<PathResult>, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let from_idx = shared::resolve_node_or_err_for_workspace_seed(&graph, from, workspace)?;
        let to_idx = shared::resolve_node_or_err_for_workspace_seed(&graph, to, workspace)?;
        let path = match graph.shortest_path(from_idx, to_idx, max_depth) {
            Some(p) => p,
            None => return Ok(None),
        };
        let mut hops = Vec::with_capacity(path.len());
        for window in path.windows(2) {
            let (src, dst) = (window[0], window[1]);
            let edge_kind = graph
                .graph()
                .edges_directed(src, petgraph::Direction::Outgoing)
                .find(|edge| edge.target() == dst)
                .map(|edge| format!("{:?}", edge.weight().kind))
                .unwrap_or_else(|| "unknown".to_string());
            let dst_node = graph.node(dst);
            hops.push(PathHop {
                key: format_node_key(&dst_node.id),
                uid: format_node_key(&dst_node.id),
                edge_kind,
            });
        }
        Ok(Some(PathResult {
            from: format_node_key(&graph.node(from_idx).id),
            to: format_node_key(&graph.node(to_idx).id),
            length: hops.len(),
            hops,
        }))
    }

    pub(super) async fn edges(
        &self,
        ctx: &ProjectCtx,
        from_glob: &str,
        to_glob: &str,
        edge_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EdgeEntry>, String> {
        use globset::Glob;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let from_matcher = Glob::new(from_glob)
            .map_err(|e| format!("invalid from_glob '{from_glob}': {e}"))?
            .compile_matcher();
        let to_matcher = Glob::new(to_glob)
            .map_err(|e| format!("invalid to_glob '{to_glob}': {e}"))?
            .compile_matcher();
        let mut out = Vec::new();
        for edge_ref in graph.graph().edge_references() {
            let src_node = graph.node(edge_ref.source());
            let dst_node = graph.node(edge_ref.target());
            let src_key = format_node_key(&src_node.id);
            let dst_key = format_node_key(&dst_node.id);
            let src_match_target = src_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| src_node.display_name.clone());
            let dst_match_target = dst_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| dst_node.display_name.clone());
            if !from_matcher.is_match(&src_match_target) {
                continue;
            }
            if !to_matcher.is_match(&dst_match_target) {
                continue;
            }
            let kind_label = format!("{:?}", edge_ref.weight().kind);
            if let Some(filter) = edge_kind
                && !kind_label.eq_ignore_ascii_case(filter)
            {
                continue;
            }
            // PR s6ch / 92z7: stamp the route-exclusion policy reason
            // on `Fetches` edges the active project policy downgrades
            // to a suggestion. The target node is the route the caller
            // is fetching — we pull its path from the graph layer and
            // run the same helpers `impact_bfs_with_policy` uses.
            let exclusion_reason: Option<String> = {
                use djinn_graph::repo_graph::{RepoGraphEdgeKind, RepoGraphNodeKind};
                if djinn_graph::route_extraction::route_parity_enabled()
                    && edge_ref.weight().kind == RepoGraphEdgeKind::Fetches
                    && dst_node.kind == RepoGraphNodeKind::Route
                {
                    let route_path = super::shared::route_node_path(dst_node);
                    let cfg = graph.route_exclusion_config();
                    super::shared::first_exclusion_reason(
                        edge_ref.weight(),
                        route_path.as_deref(),
                        cfg,
                    )
                    .map(|s| s.to_string())
                } else {
                    None
                }
            };
            out.push(EdgeEntry {
                from: src_key,
                to: dst_key,
                edge_kind: kind_label,
                edge_weight: edge_ref.weight().weight,
                confidence: edge_ref.weight().confidence,
                confidence_tier: format!("{:?}", edge_ref.weight().confidence_tier())
                    .to_ascii_lowercase(),
                reason: edge_ref.weight().reason.clone(),
                exclusion_reason,
            });
            if out.len() >= limit {
                break;
            }
        }

        // Proposal qoxm: commit co-change edges live in a sidecar OUTSIDE the
        // petgraph, so the loop above never emits them — traversal/edge ops
        // exclude co-change by default and never inflate their result set.
        // Opt in explicitly via the edge-kind filter (`edge_kind=CoChangedWith`):
        // when requested, append the file↔file co-change edges whose endpoints
        // match the globs, carrying the coupling score as confidence and the
        // temporal `last_co_change` in the reason.
        {
            use djinn_graph::repo_graph::{RepoGraphEdgeKind, edge_confidence_tier};
            let cochange_label = format!("{:?}", RepoGraphEdgeKind::CoChangedWith);
            let cochange_requested = edge_kind
                .map(|f| f.eq_ignore_ascii_case(&cochange_label))
                .unwrap_or(false);
            if cochange_requested {
                for cc in graph.cochange_edges() {
                    if out.len() >= limit {
                        break;
                    }
                    let src_node = graph.node(cc.source);
                    let dst_node = graph.node(cc.target);
                    let src_target = src_node
                        .file_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| src_node.display_name.clone());
                    let dst_target = dst_node
                        .file_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| dst_node.display_name.clone());
                    if !from_matcher.is_match(&src_target) || !to_matcher.is_match(&dst_target) {
                        continue;
                    }
                    let reason = djinn_graph::cochange::encode_reason(cc.last_co_change);
                    out.push(EdgeEntry {
                        from: format_node_key(&src_node.id),
                        to: format_node_key(&dst_node.id),
                        edge_kind: cochange_label.clone(),
                        // Co-change edges carry the coupling score as their soft
                        // weight (they have no structural SCIP evidence weight).
                        edge_weight: cc.confidence,
                        confidence: cc.confidence,
                        confidence_tier: format!(
                            "{:?}",
                            edge_confidence_tier(
                                RepoGraphEdgeKind::CoChangedWith,
                                cc.confidence,
                                Some(&reason),
                            )
                        )
                        .to_ascii_lowercase(),
                        reason: Some(reason),
                        exclusion_reason: None,
                    });
                }
            }
        }
        Ok(out)
    }
}
