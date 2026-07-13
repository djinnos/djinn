use std::sync::Arc;

use djinn_control_plane::bridge::{
    CoordinatorOps, SlotPoolOps, SnapshotEdge, SnapshotLevel, SnapshotNode, SnapshotPayload,
};
use petgraph::visit::EdgeRef;

use djinn_graph::repo_graph::RepoGraphEdgeKind;

use super::bridges::{CoordinatorBridge, LspBridge, SlotPoolBridge};
use super::graph_neighbors::format_node_key;

/// Ceiling on the number of *drawable* (non-containment) edges shipped in a
/// snapshot. A full graph can carry ~100k edges — overwhelmingly low-value
/// `FileReference` links — which dominates the wire payload (tens of MB) and
/// the client's `JSON.parse` + graph-build cost on cold load. The frontend
/// only ever renders a salience-capped subset anyway (see `MAX_RENDERED_EDGES`
/// in `ui/src/lib/codeGraphAdapter.ts`), so shipping more is pure waste.
/// Containment and cross-workspace edges are kept regardless (see
/// `build_snapshot_payload`); this caps only the cappable remainder.
pub(crate) const SNAPSHOT_DRAWABLE_EDGE_CAP: usize = 12_000;

/// Containment edges express structural nesting (a file/type contains a
/// definition/member). The UI converts them into parent/child nesting
/// metadata rather than drawing them, so they must survive the edge cap
/// whenever both endpoints are present. Mirrors `CONTAINMENT_EDGE_KINDS`
/// in `ui/src/lib/codeGraphAdapter.ts`.
fn is_containment_edge_kind(kind: RepoGraphEdgeKind) -> bool {
    matches!(
        kind,
        RepoGraphEdgeKind::ContainsDefinition
            | RepoGraphEdgeKind::DeclaredInFile
            | RepoGraphEdgeKind::MemberOf
    )
}

/// Salience used to rank drawable edges when the payload exceeds
/// `SNAPSHOT_DRAWABLE_EDGE_CAP`. Mirrors the frontend's edge-`size` factors
/// (`EDGE_STYLES` per-kind multiplier × confidence factor in
/// `ui/src/lib/codeGraphAdapter.ts`) so the server keeps the same edges the
/// client would have kept — the per-kind weight (structural/OOP spine over
/// the call graph) times per-edge confidence.
fn drawable_edge_salience(kind: RepoGraphEdgeKind, confidence: f64) -> f64 {
    let multiplier = match kind {
        RepoGraphEdgeKind::Extends => 1.0,
        RepoGraphEdgeKind::Implements => 0.9,
        RepoGraphEdgeKind::SymbolReference => 0.8,
        RepoGraphEdgeKind::EntryPointOf | RepoGraphEdgeKind::StepInProcess => 0.7,
        RepoGraphEdgeKind::FileReference | RepoGraphEdgeKind::Writes => 0.6,
        _ => 0.5,
    };
    multiplier * (0.4 + 0.6 * confidence)
}
use super::memory_enrichment::MemoryEnrichmentBridge;
use super::{RepoGraphBridge, shared};
use crate::server::AppState;

impl AppState {
    /// Helper for graph handlers in this module: compiles a
    /// [`GraphExclusions`] predicate for the given project id,
    /// falling back to the empty (Tier 1 only) filter on any DB /
    /// lookup failure.
    pub(crate) async fn mcp_state_graph_exclusions(
        &self,
        project_id: &str,
    ) -> djinn_control_plane::tools::graph_exclusions::GraphExclusions {
        use djinn_control_plane::tools::graph_exclusions::GraphExclusions;
        let repo = djinn_db::ProjectRepository::new(self.db().clone(), self.event_bus());
        match repo.get_config(project_id).await {
            Ok(Some(c)) => GraphExclusions::from_config(&c),
            _ => GraphExclusions::empty(),
        }
    }
}

impl AppState {
    /// Build a `djinn_control_plane::McpState` from this AppState, wiring all bridge impls.
    ///
    /// Snapshots the current coordinator and pool handles via `try_lock()`.
    /// In production this is called after `initialize_agents()`, so both are
    /// populated. In tests neither is initialised; tools return graceful errors.
    pub fn mcp_state(&self) -> djinn_control_plane::McpState {
        let coordinator = self.coordinator_sync().map(|c| {
            Arc::new(CoordinatorBridge {
                handle: c,
                db: self.db().clone(),
            }) as Arc<dyn CoordinatorOps>
        });
        let pool = self
            .pool_sync()
            .map(|p| Arc::new(SlotPoolBridge(p)) as Arc<dyn SlotPoolOps>);

        // Memory enrichment bridge: the agent owns the algorithm, the
        // server closes the loop between `djinn-control-plane` (the
        // consumer) and `djinn-agent` (the algorithm owner). `with_enrichment`
        // takes the bridge as `Option<Arc<dyn MemoryEnrichmentOps>>`; the
        // production wiring is always `Some`, while test harnesses that
        // don't want to plumb the bridge pass `None`.
        let enrichment_ops: Arc<dyn djinn_control_plane::bridge::MemoryEnrichmentOps> =
            Arc::new(MemoryEnrichmentBridge::new(self.db().clone()));

        djinn_control_plane::McpState::with_enrichment(
            self.db().clone(),
            self.event_bus(),
            self.catalog().clone(),
            self.health_tracker().clone(),
            self.retrieval_config(),
            self.retrieval_metrics(),
            coordinator,
            pool,
            Some(Arc::new(self.embedding_service().clone())),
            Some(self.note_vector_store()),
            Arc::new(LspBridge(self.lsp().clone())),
            Arc::new(self.clone()),
            Arc::new(self.clone()),
            Arc::new(RepoGraphBridge::new(self.clone())),
            Some(enrichment_ops),
        )
    }
}

/// PR D2: pure helper that builds a `SnapshotPayload` from an already-
/// loaded canonical graph + ranking, applying the project's
/// `graph_excluded_paths` filter and capping the surviving population
/// at `node_cap`. Unscoped multi-workspace snapshots reserve enough of
/// that cap to show each non-empty workspace when possible, then fill the
/// remainder from the global ranking; scoped snapshots remain hard-filtered
/// to the requested workspace.
///
/// Extracted from `RepoGraphBridge::snapshot` so unit tests can exercise
/// the truncation / exclusion / wire-shape logic without spinning up
/// the full bridge (which needs `AppState`, a Dolt connection, and a
/// warmed K8s job).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_snapshot_payload(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    ranking: &djinn_graph::repo_graph::RepoGraphRanking,
    project_id: String,
    git_head: String,
    generated_at: String,
    exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    workspace: Option<&str>,
    level: SnapshotLevel,
    node_cap: usize,
) -> SnapshotPayload {
    if level == SnapshotLevel::Community {
        return build_community_snapshot_payload(
            graph,
            ranking,
            project_id,
            git_head,
            generated_at,
            exclusions,
            workspace,
            node_cap,
        );
    }

    use std::collections::{BTreeMap, HashMap, HashSet};

    let workspace_prefix = shared::active_workspace_prefix(graph, workspace);

    // Tally totals against the post-exclusion graph so the
    // truncation decision lines up with what the UI actually sees.
    let mut total_nodes_post_excl: usize = 0;
    let mut eligible_nodes: Vec<petgraph::graph::NodeIndex> = Vec::new();
    let mut pagerank_lookup: HashMap<petgraph::graph::NodeIndex, f64> = HashMap::new();
    for ranked_node in &ranking.nodes {
        let node = graph.node(ranked_node.node_index);
        if node.is_route_or_tool() {
            continue;
        }
        let key = format_node_key(&node.id);
        let display_name = node.display_name.as_str();
        let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
        if exclusions.excludes(&key, file_hint.as_deref(), display_name) {
            continue;
        }
        // v8: skip external nodes from the snapshot too — they're
        // imported library symbols, not part of the codebase the UI
        // is rendering.
        if node.is_external {
            continue;
        }
        if let Some(prefix) = workspace_prefix.as_deref()
            && !shared::repo_graph_node_matches_workspace(node, prefix)
        {
            continue;
        }
        total_nodes_post_excl += 1;
        pagerank_lookup.insert(ranked_node.node_index, ranked_node.page_rank);
        // `ranking.nodes` is fused-rank-sorted (PR F4 RRF); preserve that
        // order as the global fill order after any fair workspace seeding.
        eligible_nodes.push(ranked_node.node_index);
    }

    // The ranking is built only from indexed nodes; if the graph
    // contains nodes that didn't make it into `ranking.nodes` (rare —
    // typically file nodes without symbols), fall back to a direct
    // walk. PageRank for those nodes is 0.0.
    for idx in graph.graph().node_indices() {
        if pagerank_lookup.contains_key(&idx) {
            continue;
        }
        let node = graph.node(idx);
        if node.is_route_or_tool() {
            continue;
        }
        let key = format_node_key(&node.id);
        let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
        if exclusions.excludes(&key, file_hint.as_deref(), &node.display_name) {
            continue;
        }
        if let Some(prefix) = workspace_prefix.as_deref()
            && !shared::repo_graph_node_matches_workspace(node, prefix)
        {
            continue;
        }
        total_nodes_post_excl += 1;
        pagerank_lookup.insert(idx, 0.0);
        eligible_nodes.push(idx);
    }

    let mut surviving: HashSet<petgraph::graph::NodeIndex> = HashSet::new();

    if workspace_prefix.is_none() {
        // Unscoped multi-workspace snapshots must not starve quiet workspaces
        // simply because one workspace dominates global PageRank. When the cap
        // can represent all non-empty workspaces, seed one top-ranked node per
        // workspace in slug order, then use the global ranking for the
        // remaining budget. BTreeMap gives deterministic slug tie-breaking.
        let mut by_workspace: BTreeMap<String, Vec<petgraph::graph::NodeIndex>> = BTreeMap::new();
        for &idx in &eligible_nodes {
            let node = graph.node(idx);
            let slug = node
                .workspace
                .as_deref()
                .and_then(|slug| shared::normalize_workspace_slug(Some(slug)))
                .unwrap_or_else(|| "root".to_string());
            by_workspace.entry(slug).or_default().push(idx);
        }
        if by_workspace.len() > 1 && node_cap >= by_workspace.len() {
            for nodes in by_workspace.values() {
                if surviving.len() >= node_cap {
                    break;
                }
                if let Some(&idx) = nodes.first() {
                    surviving.insert(idx);
                }
            }
        }
    }

    for idx in eligible_nodes.iter().copied() {
        if surviving.len() >= node_cap {
            break;
        }
        surviving.insert(idx);
    }

    // Cross-workspace edges are the edges the workspace zoom UI most needs to
    // see. A plain top-N induced subgraph can select one high-rank endpoint and
    // drop the lower-rank endpoint from a quieter workspace, which makes the
    // edge vanish entirely. After the initial cap, rescue both endpoints of any
    // cross-workspace edge touching the selected set. This intentionally may
    // emit slightly more nodes than `node_cap`; `node_cap` remains the initial
    // top-N budget echoed to callers, while `truncated` still tells them the
    // full post-exclusion graph was larger than the initial cap.
    for edge_ref in graph.graph().edge_references() {
        let source = edge_ref.source();
        let target = edge_ref.target();
        if !pagerank_lookup.contains_key(&source) || !pagerank_lookup.contains_key(&target) {
            continue;
        }
        let source_workspace = graph.node(source).workspace.as_deref();
        let target_workspace = graph.node(target).workspace.as_deref();
        if source_workspace == target_workspace {
            continue;
        }
        if surviving.contains(&source) || surviving.contains(&target) {
            surviving.insert(source);
            surviving.insert(target);
        }
    }

    let truncated = total_nodes_post_excl > node_cap;

    // Materialize snapshot nodes in pagerank-sorted order so the wire
    // payload is deterministic and the UI can render
    // highest-importance nodes first if it streams.
    let mut snapshot_nodes: Vec<SnapshotNode> = surviving
        .iter()
        .map(|&idx| {
            let node = graph.node(idx);
            let pagerank = pagerank_lookup.get(&idx).copied().unwrap_or(0.0);
            // 2026-04-28: prettify SCIP descriptors at the wire boundary so
            // external/cross-package symbols (`scip-go gomod ...`) reach the
            // UI as the trailing identifier (`Context`, `Errorf()`, …)
            // instead of the raw 100-char descriptor. Pure display names
            // (already-resolved symbols, file paths) pass through unchanged.
            // The UI keeps a defensive `prettifyLabel` mirror in case a
            // future snapshot path forgets to call this — see
            // `djinn_graph::scip_parser::prettify_scip_descriptor`.
            let label = djinn_graph::scip_parser::prettify_scip_descriptor(&node.display_name);
            SnapshotNode {
                id: format_node_key(&node.id),
                uid: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                label,
                workspace: node.workspace.clone(),
                workspace_kind: None,
                member_count: None,
                internal_edge_count: None,
                symbol_kind: node
                    .symbol_kind
                    .as_ref()
                    .map(|k| format!("{k:?}").to_lowercase()),
                file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
                pagerank,
                // PR F3: populate from the canonical graph's community
                // sidecar; `None` when the node is a singleton (not in
                // any non-trivial community) or when detection was
                // skipped (`DJINN_COMMUNITY_DETECTION=0`).
                community_id: graph.community_id(idx).map(str::to_string),
                // Iter 30: per-function cognitive complexity from the
                // tree-sitter walker (iter 26 post-pass). Drives the
                // `/code-graph` heatmap overlay; `None` for non-function
                // nodes and unsupported languages — the UI maps null →
                // muted gray so non-function nodes don't dominate.
                cognitive: node.complexity.map(|c| c.cognitive),
                // v10: canonical test flag (file-path convention OR SCIP
                // Test role), stamped at graph-build time.
                is_test: node.is_test,
                // 7e6o: warm-time layout coordinates from the deterministic
                // community-aware cache (djinn-graph). The sidecar is
                // populated during warm and backfilled on legacy-artifact
                // load, so `unwrap_or(0.0, 0.0)` only fires for synthetic
                // nodes that never had a position computed.
                x: graph.layout_position(idx).map(|p| p.x).unwrap_or_default(),
                y: graph.layout_position(idx).map(|p| p.y).unwrap_or_default(),
                keywords: Vec::new(),
            }
        })
        .collect();
    snapshot_nodes.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    // Walk every edge in the underlying petgraph; keep only those
    // whose source AND target survived the cap. `total_edges` is
    // the post-exclusion count (we drop edges that touch excluded
    // nodes so the totals match the visible graph).
    //
    // Edge payload cap (perf): keep every containment edge (the UI needs
    // them to nest symbols in files) and every cross-workspace edge (the
    // few highlighted inter-module links), then cap the remaining drawable
    // edges to the highest `SNAPSHOT_DRAWABLE_EDGE_CAP` by salience. This
    // is what makes the snapshot small enough to parse on cold load; the
    // frontend re-applies the same salience cap. `total_edges` still
    // reports the full post-exclusion count so the UI shows "N of M".
    let mut total_edges_post_excl: usize = 0;
    let mut containment_edges: Vec<SnapshotEdge> = Vec::new();
    let mut cross_workspace_edges: Vec<SnapshotEdge> = Vec::new();
    // (salience, edge) for the cappable intra-workspace drawable edges.
    let mut drawable_edges: Vec<(f64, SnapshotEdge)> = Vec::new();
    for edge_ref in graph.graph().edge_references() {
        let src_in = pagerank_lookup.contains_key(&edge_ref.source());
        let dst_in = pagerank_lookup.contains_key(&edge_ref.target());
        if !src_in || !dst_in {
            continue;
        }
        total_edges_post_excl += 1;
        if !surviving.contains(&edge_ref.source()) || !surviving.contains(&edge_ref.target()) {
            continue;
        }
        let from_node = graph.node(edge_ref.source());
        let to_node = graph.node(edge_ref.target());
        let weight = edge_ref.weight();
        let edge = SnapshotEdge {
            from: format_node_key(&from_node.id),
            to: format_node_key(&to_node.id),
            kind: format!("{:?}", weight.kind),
            confidence: weight.confidence,
            reason: weight.reason.clone(),
        };
        if is_containment_edge_kind(weight.kind) {
            containment_edges.push(edge);
        } else if from_node.workspace.is_some()
            && to_node.workspace.is_some()
            && from_node.workspace != to_node.workspace
        {
            cross_workspace_edges.push(edge);
        } else {
            drawable_edges.push((drawable_edge_salience(weight.kind, weight.confidence), edge));
        }
    }

    // Cap the cappable remainder to the budget left after the always-kept
    // cross-workspace edges, keeping the highest-salience edges.
    let intra_budget = SNAPSHOT_DRAWABLE_EDGE_CAP.saturating_sub(cross_workspace_edges.len());
    if drawable_edges.len() > intra_budget {
        drawable_edges.select_nth_unstable_by(intra_budget, |a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        drawable_edges.truncate(intra_budget);
    }

    let mut snapshot_edges: Vec<SnapshotEdge> = Vec::with_capacity(
        containment_edges.len() + cross_workspace_edges.len() + drawable_edges.len(),
    );
    snapshot_edges.append(&mut containment_edges);
    snapshot_edges.append(&mut cross_workspace_edges);
    snapshot_edges.extend(drawable_edges.into_iter().map(|(_, edge)| edge));

    // Sort edges deterministically (kind > from > to) so test snapshots
    // stay stable across runs.
    snapshot_edges.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.from.cmp(&b.from))
            .then_with(|| a.to.cmp(&b.to))
    });

    SnapshotPayload {
        project_id,
        git_head,
        generated_at,
        truncated,
        total_nodes: total_nodes_post_excl,
        total_edges: total_edges_post_excl,
        node_cap,
        nodes: snapshot_nodes,
        edges: snapshot_edges,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_community_snapshot_payload(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    ranking: &djinn_graph::repo_graph::RepoGraphRanking,
    project_id: String,
    git_head: String,
    generated_at: String,
    exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    workspace: Option<&str>,
    node_cap: usize,
) -> SnapshotPayload {
    use petgraph::visit::EdgeRef;
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

    struct CommunityAgg {
        id: String,
        label: String,
        members: BTreeSet<petgraph::graph::NodeIndex>,
        workspaces: BTreeSet<String>,
        missing_workspace: bool,
        pagerank_sum: f64,
        internal_edges: usize,
        keywords: Vec<String>,
    }

    let workspace_prefix = shared::active_workspace_prefix(graph, workspace);
    let community_meta: HashMap<&str, &djinn_graph::communities::Community> = graph
        .communities()
        .iter()
        .map(|community| (community.id.as_str(), community))
        .collect();
    let mut pagerank_lookup: HashMap<petgraph::graph::NodeIndex, f64> = HashMap::new();
    for ranked_node in &ranking.nodes {
        pagerank_lookup.insert(ranked_node.node_index, ranked_node.page_rank);
    }

    let mut eligible_nodes: HashSet<petgraph::graph::NodeIndex> = HashSet::new();
    let mut communities: BTreeMap<String, CommunityAgg> = BTreeMap::new();
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        let key = format_node_key(&node.id);
        let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
        if node.is_external || exclusions.excludes(&key, file_hint.as_deref(), &node.display_name) {
            continue;
        }
        if let Some(prefix) = workspace_prefix.as_deref()
            && !shared::repo_graph_node_matches_workspace(node, prefix)
        {
            continue;
        }
        let Some(community_id) = graph.community_id(idx) else {
            continue;
        };

        eligible_nodes.insert(idx);
        let meta = community_meta.get(community_id).copied();
        let agg = communities
            .entry(community_id.to_string())
            .or_insert_with(|| CommunityAgg {
                id: community_id.to_string(),
                label: meta
                    .map(|community| community.label.clone())
                    .unwrap_or_else(|| community_id.to_string()),
                members: BTreeSet::new(),
                workspaces: BTreeSet::new(),
                missing_workspace: false,
                pagerank_sum: 0.0,
                internal_edges: 0,
                keywords: meta
                    .map(|community| community.keywords.clone())
                    .unwrap_or_default(),
            });
        agg.members.insert(idx);
        if let Some(workspace) = node.workspace.as_deref() {
            agg.workspaces.insert(workspace.to_string());
        } else {
            agg.missing_workspace = true;
        }
        agg.pagerank_sum += pagerank_lookup.get(&idx).copied().unwrap_or(0.0);
    }

    type CommunityEdgeKey = (String, String, String);
    type CommunityEdgeAgg = (f64, Option<String>, usize);
    let mut edge_aggs: BTreeMap<CommunityEdgeKey, CommunityEdgeAgg> = BTreeMap::new();
    let mut total_inter_community_edges = 0usize;
    for edge_ref in graph.graph().edge_references() {
        let source = edge_ref.source();
        let target = edge_ref.target();
        if !eligible_nodes.contains(&source) || !eligible_nodes.contains(&target) {
            continue;
        }
        let Some(source_community) = graph.community_id(source) else {
            continue;
        };
        let Some(target_community) = graph.community_id(target) else {
            continue;
        };
        if source_community == target_community {
            if let Some(agg) = communities.get_mut(source_community) {
                agg.internal_edges += 1;
            }
            continue;
        }
        total_inter_community_edges += 1;
        let weight = edge_ref.weight();
        let key = (
            source_community.to_string(),
            target_community.to_string(),
            format!("{:?}", weight.kind),
        );
        let entry = edge_aggs.entry(key).or_insert((0.0, None, 0));
        entry.0 += weight.confidence;
        if entry.1.is_none() {
            entry.1 = weight.reason.clone();
        }
        entry.2 += 1;
    }

    let mut snapshot_nodes: Vec<SnapshotNode> = communities
        .into_values()
        .filter(|agg| !agg.members.is_empty())
        .map(|agg| {
            let (workspace, workspace_kind) = match (agg.workspaces.len(), agg.missing_workspace) {
                (1, false) => (agg.workspaces.iter().next().cloned(), "single".to_string()),
                (0, true) => (None, "unknown".to_string()),
                _ => (None, "mixed".to_string()),
            };
            // 7e6o: community aggregate coordinate. The warm-time layout
            // cache is keyed by stable node UID, not by community id, so
            // the community super-node position is the deterministic
            // centroid of its finite member coordinates. Members with no
            // cached position are skipped; if none have coordinates the
            // centroid falls back to (0.0, 0.0).
            let (x, y) = community_centroid(graph, &agg.members);
            SnapshotNode {
                id: agg.id.clone(),
                uid: agg.id.clone(),
                kind: "community".to_string(),
                label: agg.label,
                workspace,
                workspace_kind: Some(workspace_kind),
                member_count: Some(agg.members.len()),
                internal_edge_count: Some(agg.internal_edges),
                symbol_kind: None,
                file_path: None,
                pagerank: agg.pagerank_sum,
                community_id: Some(agg.id),
                cognitive: None,
                is_test: false,
                x,
                y,
                keywords: agg.keywords,
            }
        })
        .collect();
    snapshot_nodes.sort_by(|a, b| {
        b.pagerank
            .total_cmp(&a.pagerank)
            .then_with(|| a.id.cmp(&b.id))
    });

    let emitted_ids: HashSet<&str> = snapshot_nodes.iter().map(|node| node.id.as_str()).collect();
    let mut snapshot_edges: Vec<SnapshotEdge> = edge_aggs
        .into_iter()
        .filter_map(|((from, to, kind), (confidence_sum, reason, count))| {
            if !emitted_ids.contains(from.as_str()) || !emitted_ids.contains(to.as_str()) {
                return None;
            }
            Some(SnapshotEdge {
                from,
                to,
                kind,
                confidence: (confidence_sum / count as f64).clamp(0.0, 1.0),
                reason,
            })
        })
        .collect();
    snapshot_edges.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.from.cmp(&b.from))
            .then_with(|| a.to.cmp(&b.to))
    });

    SnapshotPayload {
        project_id,
        git_head,
        generated_at,
        truncated: false,
        total_nodes: snapshot_nodes.len(),
        total_edges: total_inter_community_edges,
        node_cap,
        nodes: snapshot_nodes,
        edges: snapshot_edges,
    }
}

/// 7e6o: compute a deterministic community super-node coordinate as the
/// centroid of the finite warm-time member positions. Members without a
/// cached layout position are skipped so a single sparse member cannot
/// skew the centre. Returns `(0.0, 0.0)` when no member has coordinates.
fn community_centroid(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    members: &std::collections::BTreeSet<petgraph::graph::NodeIndex>,
) -> (f64, f64) {
    let mut sum_x = 0.0_f64;
    let mut sum_y = 0.0_f64;
    let mut count = 0_u32;
    for &idx in members {
        if let Some(pos) = graph.layout_position(idx)
            && pos.x.is_finite()
            && pos.y.is_finite()
        {
            sum_x += pos.x;
            sum_y += pos.y;
            count += 1;
        }
    }
    if count == 0 {
        (0.0, 0.0)
    } else {
        (sum_x / count as f64, sum_y / count as f64)
    }
}

#[cfg(test)]
mod edge_cap_tests {
    use super::*;

    #[test]
    fn containment_kinds_are_classified_as_containment() {
        for kind in [
            RepoGraphEdgeKind::ContainsDefinition,
            RepoGraphEdgeKind::DeclaredInFile,
            RepoGraphEdgeKind::MemberOf,
        ] {
            assert!(
                is_containment_edge_kind(kind),
                "{kind:?} should be containment"
            );
        }
        for kind in [
            RepoGraphEdgeKind::FileReference,
            RepoGraphEdgeKind::SymbolReference,
            RepoGraphEdgeKind::Extends,
            RepoGraphEdgeKind::EntryPointOf,
        ] {
            assert!(
                !is_containment_edge_kind(kind),
                "{kind:?} should be drawable"
            );
        }
    }

    #[test]
    fn salience_ranks_structural_over_file_refs_and_rewards_confidence() {
        // OOP spine outranks the file-reference wall at equal confidence,
        // matching the frontend's EDGE_STYLES multipliers.
        let extends = drawable_edge_salience(RepoGraphEdgeKind::Extends, 1.0);
        let file_ref = drawable_edge_salience(RepoGraphEdgeKind::FileReference, 1.0);
        assert!(
            extends > file_ref,
            "Extends ({extends}) should outrank FileReference ({file_ref})"
        );

        // Higher confidence wins within a kind.
        let hi = drawable_edge_salience(RepoGraphEdgeKind::FileReference, 0.95);
        let lo = drawable_edge_salience(RepoGraphEdgeKind::FileReference, 0.3);
        assert!(hi > lo, "higher confidence should rank higher");
    }
}
