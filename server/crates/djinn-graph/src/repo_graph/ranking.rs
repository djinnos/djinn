//! PageRank, entry-point distance, RRF fusion, and the
//! `RepoGraphRanking` / `RankedRepoGraphNode` data types.
//!
//! The actual [`RepoDependencyGraph`](super::RepoDependencyGraph) lives
//! in `super::graph`; this module only owns the ranking math and the
//! data shapes that `rank()` returns.

use petgraph::Direction::{Incoming, Outgoing};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use super::edge::{RepoGraphEdge, RepoGraphEdgeKind};
use super::node::{RepoGraphNode, RepoGraphNodeKind, RepoNodeKey};

/// Reusable centrality/noise predicate for synthetic Route/Tool affordance nodes.
///
/// Route and Tool nodes are anchors for inferred edges, not architecture hubs in
/// their own right.  Keep them in the graph so PageRank can flow through their
/// edges, but filter them out before any `ranked`/god-object/high-coupling
/// diagnostic projects node centrality to users.
pub(crate) fn is_route_or_tool_node(node: &RepoGraphNode) -> bool {
    node.is_route_or_tool()
}

/// Returns true when a `Route` node is backed by only one extraction evidence
/// source and has no consumer/client edges.
///
/// Route nodes do not currently carry node-level evidence counts, so ranking
/// filters derive the effective count from adjacent route-extraction edges.
/// `HandlesRoute`/`Route` edges represent route evidence; incoming `Fetches`
/// edges represent consumers and disqualify the singleton/no-consumer case.
pub fn is_singleton_route_without_consumers(
    graph: &DiGraph<RepoGraphNode, RepoGraphEdge>,
    node_index: NodeIndex,
) -> bool {
    let Some(node) = graph.node_weight(node_index) else {
        return false;
    };
    if node.kind != RepoGraphNodeKind::Route || !matches!(node.id, RepoNodeKey::Route(_)) {
        return false;
    }

    let mut evidence_count = 0usize;
    let mut has_consumers = false;

    for edge in graph.edges_directed(node_index, Incoming) {
        match edge.weight().kind {
            RepoGraphEdgeKind::Fetches => has_consumers = true,
            RepoGraphEdgeKind::HandlesRoute | RepoGraphEdgeKind::Route => {
                evidence_count += edge.weight().evidence_count.max(1);
            }
            _ => {}
        }
    }
    for edge in graph.edges_directed(node_index, Outgoing) {
        if matches!(
            edge.weight().kind,
            RepoGraphEdgeKind::HandlesRoute | RepoGraphEdgeKind::Route
        ) {
            evidence_count += edge.weight().evidence_count.max(1);
        }
    }

    evidence_count == 1 && !has_consumers
}

/// Standard sparse PageRank, O((V + E) × iterations) per full pass.
///
/// Replaces `petgraph::algo::page_rank`, whose 0.8.x implementation is
/// O(V² × avg_out_degree) per iteration because its inner loop scans
/// every (v, w) pair and, for each pair, walks `w`'s out-edges looking
/// for `v`.  On this repo's canonical graph (≈12 k nodes, ≈150 k edges,
/// 25 iterations) that worked out to ~45 billion edge comparisons →
/// ~37 minutes of wall-clock on a warm cache rebuild (observed in
/// `ensure_canonical_graph: build pipeline complete` metrics on
/// 2026-04-08).  The sparse pass below does ~4 million ops total for
/// the same workload.
///
/// Formula (standard Google PageRank with dangling mass redistribution):
///
/// ```text
/// r_{k+1}(v) = (1 − d) / N
///            + d × ( Σ (r_k(u) / outdeg(u))  for u in in(v) )
///            + d × ( dangling_sum_k / N )
/// ```
///
/// where `dangling_sum_k` is the total rank mass held by nodes with
/// zero out-edges at iteration `k`.  Ranks are re-normalized every
/// iteration to correct floating-point drift.
///
/// The return vector is indexed by `NodeIndex::index()`, matching the
/// layout `petgraph::algo::page_rank` produced, so existing callers
/// in `rank()` need no other changes.
pub(crate) fn compute_pagerank_sparse(
    graph: &DiGraph<RepoGraphNode, RepoGraphEdge>,
    damping: f64,
    iterations: usize,
) -> Vec<f64> {
    let n = graph.node_count();
    if n == 0 {
        return Vec::new();
    }
    let n_f = n as f64;
    let initial = 1.0 / n_f;
    let mut ranks = vec![initial; n];

    // Precompute out-degree per node index.  Dangling nodes get 0 and
    // are handled specially below.
    let mut out_degree = vec![0u32; n];
    for node_idx in graph.node_indices() {
        out_degree[node_idx.index()] = graph.edges_directed(node_idx, Outgoing).count() as u32;
    }

    let random_jump = (1.0 - damping) / n_f;

    for _ in 0..iterations {
        // Sum the rank held by dangling nodes — that mass is
        // redistributed uniformly across all nodes so PageRank remains
        // mass-preserving even when some nodes have no out-edges.
        let mut dangling_sum = 0.0;
        for u in 0..n {
            if out_degree[u] == 0 {
                dangling_sum += ranks[u];
            }
        }
        let dangling_contribution = damping * dangling_sum / n_f;
        let baseline = random_jump + dangling_contribution;

        let mut new_ranks = vec![baseline; n];

        // For each source node with at least one out-edge, push its
        // share along each outgoing edge.  O(V + E) per iteration.
        for u_idx in graph.node_indices() {
            let u = u_idx.index();
            let out = out_degree[u];
            if out == 0 {
                continue; // already captured in dangling_sum
            }
            let share = damping * ranks[u] / (out as f64);
            for edge in graph.edges_directed(u_idx, Outgoing) {
                new_ranks[edge.target().index()] += share;
            }
        }

        // Re-normalize to guard against floating-point drift.
        let sum: f64 = new_ranks.iter().sum();
        if sum > 0.0 {
            for r in &mut new_ranks {
                *r /= sum;
            }
        }

        ranks = new_ranks;
    }

    ranks
}

/// PR F4: BFS shortest hop count to every node from the entry-point
/// set. Sources (`distance = 0`) are nodes that have at least one
/// incoming `EntryPointOf` edge — i.e. the entry-point function nodes
/// themselves (`fn main`, route handlers, tests, …). The traversal
/// follows `Outgoing` edges from those sources, so dependents of
/// entry points get small distances and pure utility helpers reachable
/// only via reverse traversal are absent from the map.
///
/// Returned map omits unreachable nodes so the rank-position calculation
/// can treat `None` as "infinity" (last in the entry-distance ranking).
pub(crate) fn compute_entry_point_distance(
    graph: &DiGraph<RepoGraphNode, RepoGraphEdge>,
) -> std::collections::HashMap<NodeIndex, u32> {
    use std::collections::{HashMap, VecDeque};

    let mut distances: HashMap<NodeIndex, u32> = HashMap::new();
    let mut queue: VecDeque<NodeIndex> = VecDeque::new();
    for idx in graph.node_indices() {
        let is_entry = graph
            .edges_directed(idx, Incoming)
            .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf);
        if is_entry {
            distances.insert(idx, 0);
            queue.push_back(idx);
        }
    }
    while let Some(node) = queue.pop_front() {
        let next_dist = distances[&node].saturating_add(1);
        for edge in graph.edges_directed(node, Outgoing) {
            let target = edge.target();
            if let std::collections::hash_map::Entry::Vacant(e) = distances.entry(target) {
                e.insert(next_dist);
                queue.push_back(target);
            }
        }
    }
    distances
}

/// PR F4: Reciprocal Rank Fusion (K=60) across pagerank, total-degree,
/// and entry-point distance. Mutates `nodes` in place to set the
/// `fused_rank` field — caller is responsible for the final sort.
///
/// Rank positions are computed deterministically: `total_cmp` for the
/// numeric signals, alphabetical key as the final tiebreaker so two
/// nodes with identical raw values still get distinct positions.
pub(crate) fn apply_rrf_fused_rank(nodes: &mut [RankedRepoGraphNode]) {
    const K: f64 = 60.0;
    if nodes.is_empty() {
        return;
    }

    // PageRank desc (highest first)
    let mut by_pagerank: Vec<usize> = (0..nodes.len()).collect();
    by_pagerank.sort_by(|&a, &b| {
        nodes[b]
            .page_rank
            .total_cmp(&nodes[a].page_rank)
            .then_with(|| nodes[a].key.cmp(&nodes[b].key))
    });

    // Total degree desc
    let mut by_degree: Vec<usize> = (0..nodes.len()).collect();
    by_degree.sort_by(|&a, &b| {
        let total_a = nodes[a].inbound_edge_weight + nodes[a].outbound_edge_weight;
        let total_b = nodes[b].inbound_edge_weight + nodes[b].outbound_edge_weight;
        total_b
            .total_cmp(&total_a)
            .then_with(|| nodes[a].key.cmp(&nodes[b].key))
    });

    // Entry-point distance asc — None sorts last so nodes unreachable
    // from any entry point sit at the bottom of this signal.
    let mut by_distance: Vec<usize> = (0..nodes.len()).collect();
    by_distance.sort_by(|&a, &b| {
        let da = nodes[a].entry_point_distance;
        let db = nodes[b].entry_point_distance;
        match (da, db) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| nodes[a].key.cmp(&nodes[b].key)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => nodes[a].key.cmp(&nodes[b].key),
        }
    });

    let mut pagerank_pos = vec![0_usize; nodes.len()];
    let mut degree_pos = vec![0_usize; nodes.len()];
    let mut distance_pos = vec![0_usize; nodes.len()];
    for (rank, &orig_idx) in by_pagerank.iter().enumerate() {
        pagerank_pos[orig_idx] = rank;
    }
    for (rank, &orig_idx) in by_degree.iter().enumerate() {
        degree_pos[orig_idx] = rank;
    }
    for (rank, &orig_idx) in by_distance.iter().enumerate() {
        distance_pos[orig_idx] = rank;
    }

    for (i, node) in nodes.iter_mut().enumerate() {
        let pr = pagerank_pos[i] as f64;
        let dr = degree_pos[i] as f64;
        let er = distance_pos[i] as f64;
        node.fused_rank = (1.0 / (K + pr)) + (1.0 / (K + dr)) + (1.0 / (K + er));
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoGraphRanking {
    pub nodes: Vec<RankedRepoGraphNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedRepoGraphNode {
    pub node_index: NodeIndex,
    pub key: RepoNodeKey,
    pub kind: RepoGraphNodeKind,
    pub score: f64,
    pub page_rank: f64,
    pub structural_weight: f64,
    pub inbound_edge_weight: f64,
    pub outbound_edge_weight: f64,
    // v8: added with parse-time scoped-variable filter; see version bump
    // in sibling change. PR F4: multi-signal Reciprocal Rank Fusion
    // surfaces entry-point membership and BFS distance from the entry
    // set so utility helpers stop dominating the top of `ranked`.
    pub is_entry_point: bool,
    pub entry_point_distance: Option<u32>,
    pub fused_rank: f64,
}
