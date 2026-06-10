//! `djinn-graph::repo_graph` — in-memory repository dependency graph
//! data structure plus its builders, queries, serialization, and
//! persistence helpers.
//!
//! The module used to be a single 4,573-line `repo_graph.rs` file. The
//! follow-up tasks in this wave split it into focused submodules so
//! individual concerns can be reviewed and tested in isolation:
//!
//! | submodule  | concern (this task)                                        |
//! |------------|------------------------------------------------------------|
//! | `constants`| edge confidence/weight tables, PageRank knobs, version stamp |
//! | `node`     | `RepoGraphNode`, `RepoNodeKey`, `RepoGraphNodeKind`          |
//! | `edge`     | `RepoGraphEdge`, `RepoGraphEdgeKind`, edge weight/confidence |
//! | `tests`    | the `repo_graph::tests` test module                         |
//! | `artifact` | `RepoGraphArtifact` + v10 compat (placeholder for `yxp7`)   |
//! | `builder`  | `RepoDependencyGraphBuilder` (filled in by `3hrr`)          |
//! | `graph`    | `RepoDependencyGraph` + queries (placeholder for `our5`)    |
//! | `ranking`  | PageRank / RRF (placeholder for `our5`)                    |
//!
//! All public types are re-exported here so downstream consumers
//! (`crate::repo_graph::RepoGraphNode`, etc.) keep working without
//! edits.

mod artifact;
mod builder;
mod constants;
mod edge;
mod graph;
mod node;
mod ranking;

#[cfg(test)]
mod tests;

// Re-exports for the public API — see `crates/djinn-control-plane/src/
// tools/graph_tools.rs`, `server/src/mcp_bridge.rs`, `cluster_doc.rs`,
// `communities.rs`, etc. for the consumer side.
pub use self::constants::{REPO_GRAPH_ARTIFACT_VERSION, is_test_path};
pub use self::edge::{RepoGraphEdge, RepoGraphEdgeKind, edge_confidence_floor};
pub use self::node::{RepoGraphNode, RepoGraphNodeKind, RepoGraphSearchHit, RepoNodeKey};

// Re-exports for sibling submodule bodies (the impl blocks in
// `mod.rs` itself) AND the `repo_graph::tests` test module. `pub(crate)`
// so the items stay crate-internal while still being reachable from
// `mod.rs` and its descendants (including the test module, which is a
// child of `mod.rs`).
//
// `#[allow(unused_imports)]` because the `EDGE_CONFIDENCE_*` constants
// are only consumed by the test module — the lib build never references
// them, and the test build does. Without the attribute the lib build
// would emit an unused-imports warning.
#[allow(unused_imports)]
pub(crate) use self::constants::{
    EDGE_CONFIDENCE_LOCAL_PENALTY, EDGE_CONFIDENCE_READS, EDGE_CONFIDENCE_WRITES,
    PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS,
};
pub(crate) use self::edge::{edge_weight, edge_weight_for};

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::Direction::{Incoming, Outgoing};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef as PetgraphEdgeRef;
use serde::{Deserialize, Serialize};

use crate::complexity::{ComplexityMetrics, ComplexityWalker};
use crate::scip_parser::{ParsedScipIndex, ScipSymbolKind, ScipVisibility};

/// Stable, reusable repository dependency graph built from normalized SCIP parse output.
#[derive(Debug, Clone)]
pub struct RepoDependencyGraph {
    graph: DiGraph<RepoGraphNode, RepoGraphEdge>,
    node_lookup: BTreeMap<RepoNodeKey, NodeIndex>,
    /// Index from lowercased `display_name` to the nodes that use it.
    /// Populated at build time so `search` is O(log N + k).
    name_index: BTreeMap<String, Vec<NodeIndex>>,
    /// Per-file list of symbol-definition enclosing ranges, sorted by
    /// `start_line`. Populated by [`RepoDependencyGraph::build`] from parsed
    /// SCIP input, and round-tripped through the artifact so cache-hit
    /// reloads via [`RepoDependencyGraph::from_artifact`] retain it.
    symbol_ranges: BTreeMap<PathBuf, Vec<SymbolRange>>,
    /// PR F3: detected communities (greedy modularity over the
    /// undirected weighted projection). Populated by
    /// [`RepoDependencyGraph::build`] when `DJINN_COMMUNITY_DETECTION`
    /// is unset/true; round-tripped through the artifact so cache-hit
    /// reloads keep them.
    communities: Vec<crate::communities::Community>,
    /// Reverse index: `NodeIndex::index()` → position in `communities`.
    /// Built whenever `communities` is set (build-time or after
    /// `from_artifact`). Singleton nodes (not in any community) are
    /// absent from the map.
    community_lookup: BTreeMap<usize, usize>,
    /// PR F2: detected execution-flow processes traced from each
    /// entry point. Populated by [`RepoDependencyGraph::build`] when
    /// `DJINN_PROCESS_DETECTION` is unset/true; round-tripped through
    /// the artifact so cache-hit reloads keep them.
    processes: Vec<crate::processes::Process>,
    /// Reverse index: `NodeIndex::index()` → list of positions in
    /// `processes` where the node appears as a step. Built whenever
    /// `processes` is set (build-time or after `from_artifact`). Empty
    /// for nodes that don't participate in any traced process.
    process_lookup: BTreeMap<usize, Vec<usize>>,
}

/// A single SCIP definition range pinned to a graph node.
///
/// Line numbers are 1-indexed and inclusive on both ends, matching the
/// convention used by callers (diff hunks, editor selections).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolRange {
    pub start_line: u32,
    pub end_line: u32,
    pub node: NodeIndex,
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
fn compute_pagerank_sparse(
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
fn compute_entry_point_distance(
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
fn apply_rrf_fused_rank(nodes: &mut [RankedRepoGraphNode]) {
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

impl RepoDependencyGraph {
    pub fn build(indices: &[ParsedScipIndex]) -> Self {
        Self::build_with_source(indices, None)
    }

    /// Build with an optional project-clone root. When `project_root` is
    /// `Some`, the edge-classification path will read source files via
    /// the [`crate::access_classifier::AccessClassifier`] to recover
    /// `Reads`/`Writes` edges for indexers (notably rust-analyzer) whose
    /// SCIP output doesn't carry `ReadAccess`/`WriteAccess` role bits.
    /// Tests that don't need access classification should call
    /// [`Self::build`] (no on-disk file required).
    pub fn build_with_source(indices: &[ParsedScipIndex], project_root: Option<&Path>) -> Self {
        let mut builder = RepoDependencyGraphBuilder {
            project_root: project_root.map(|p| p.to_path_buf()),
            ..RepoDependencyGraphBuilder::default()
        };
        for index in indices {
            builder.add_index(index);
        }
        let mut graph = builder.finish();
        // PR F1: post-build entry-point detection. Stamps `EntryPointOf`
        // edges from file → symbol so `dead_symbols` (and downstream
        // F2 process tracing) can ask "is this an entry point?" via a
        // single edge query. Off-by-default escape hatch via the
        // `DJINN_ENTRY_POINT_DETECTION` env var.
        if crate::entry_points::entry_point_detection_enabled() {
            let _ = crate::entry_points::detect_entry_points(&mut graph);
        }
        // PR F2: post-entry-point process tracing. Walks each entry-
        // point's deterministic call chain and materializes a
        // `Process` synthetic node + `StepInProcess` edges. Off-by-
        // default escape hatch via the `DJINN_PROCESS_DETECTION`
        // env var. No-op when entry-point detection didn't fire.
        if crate::processes::process_detection_enabled() {
            let processes = crate::processes::detect_processes(&mut graph);
            graph.set_processes(processes);
        }
        // Iteration 26: attach per-function complexity metrics
        // (cyclomatic, cognitive, nloc, max_nesting, param_count) to
        // every function-like graph node. Reads source files from the
        // project root supplied to `build_with_source`; without a root
        // (i.e. `Self::build` for synthetic-fixture unit tests) the
        // closure short-circuits and complexity stays `None`.
        if let Some(root) = project_root.map(|p| p.to_path_buf()) {
            attach_complexity_metrics(&mut graph, |rel| {
                std::fs::read_to_string(root.join(rel)).ok()
            });
        }
        graph
    }

    pub fn graph(&self) -> &DiGraph<RepoGraphNode, RepoGraphEdge> {
        &self.graph
    }

    /// PR F1: mutable graph access scoped to the crate. Used by
    /// [`crate::entry_points::detect_entry_points`] to stamp
    /// `EntryPointOf` edges after the SCIP-driven build pass. Not
    /// exposed publicly because callers outside the crate should never
    /// need to mutate edge structure directly.
    pub(crate) fn graph_mut_unchecked(&mut self) -> &mut DiGraph<RepoGraphNode, RepoGraphEdge> {
        &mut self.graph
    }

    pub fn node(&self, index: NodeIndex) -> &RepoGraphNode {
        &self.graph[index]
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn file_node(&self, path: impl AsRef<Path>) -> Option<NodeIndex> {
        self.node_lookup
            .get(&RepoNodeKey::File(path.as_ref().to_path_buf()))
            .copied()
    }

    pub fn symbol_node(&self, symbol: &str) -> Option<NodeIndex> {
        self.node_lookup
            .get(&RepoNodeKey::Symbol(symbol.to_string()))
            .copied()
    }

    pub fn rank(&self) -> RepoGraphRanking {
        let page_rank_scores =
            compute_pagerank_sparse(&self.graph, PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS);

        // PR F4: identify entry-point nodes (any node with an incoming
        // `EntryPointOf` edge) and BFS the graph from them via Outgoing
        // edges to compute `entry_point_distance`. Distance 0 sits on
        // the entry-point function itself; downstream callees grow
        // monotonically. Unreachable nodes stay `None`.
        let entry_distance = compute_entry_point_distance(&self.graph);

        let mut scored_nodes = Vec::with_capacity(self.graph.node_count());
        for node_index in self.graph.node_indices() {
            let node = &self.graph[node_index];
            let page_rank = page_rank_scores[node_index.index()];
            let structural_weight = self.structural_weight(node_index);
            let score = page_rank * structural_weight;
            let is_entry_point = entry_distance
                .get(&node_index)
                .map(|d| *d == 0)
                .unwrap_or(false);
            scored_nodes.push(RankedRepoGraphNode {
                node_index,
                key: node.key(),
                kind: node.kind(),
                score,
                page_rank,
                structural_weight,
                inbound_edge_weight: self.total_edge_weight(node_index, Incoming),
                outbound_edge_weight: self.total_edge_weight(node_index, Outgoing),
                is_entry_point,
                entry_point_distance: entry_distance.get(&node_index).copied(),
                // Filled in by `apply_rrf_fused_rank` below — we need
                // the full ranks before we can compute it.
                fused_rank: 0.0,
            });
        }

        // PR F4: Reciprocal Rank Fusion across pagerank, total degree,
        // and entry-point distance. Sort by fused rank desc; secondary
        // tiebreakers (pagerank → structural_weight → key) match the
        // legacy ordering so deterministic snapshots stay stable when
        // two nodes happen to fuse to the same value.
        apply_rrf_fused_rank(&mut scored_nodes);

        scored_nodes.sort_by(|left, right| {
            right
                .fused_rank
                .total_cmp(&left.fused_rank)
                .then_with(|| right.page_rank.total_cmp(&left.page_rank))
                .then_with(|| right.structural_weight.total_cmp(&left.structural_weight))
                .then_with(|| left.key.cmp(&right.key))
        });

        RepoGraphRanking {
            nodes: scored_nodes,
        }
    }

    fn structural_weight(&self, node_index: NodeIndex) -> f64 {
        let node = &self.graph[node_index];
        let inbound_edge_weight = self.total_edge_weight(node_index, Incoming);
        let outbound_edge_weight = self.total_edge_weight(node_index, Outgoing);
        let degree_bonus = (inbound_edge_weight * 1.2) + (outbound_edge_weight * 0.8);
        node.intrinsic_weight() + degree_bonus
    }

    fn total_edge_weight(&self, node_index: NodeIndex, direction: petgraph::Direction) -> f64 {
        self.graph
            .edges_directed(node_index, direction)
            .map(|edge| edge.weight().weight)
            .sum()
    }

    /// Search the name index by lowercased display-name. Returns hits ranked
    /// by:
    /// 1. exact name match
    /// 2. suffix match on the display name
    /// 3. substring match
    ///
    /// then by alphabetical key for stability.
    pub fn search_by_name(
        &self,
        query: &str,
        kind_filter: Option<RepoGraphNodeKind>,
        limit: usize,
    ) -> Vec<RepoGraphSearchHit> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_lowercase();
        let mut hits: Vec<RepoGraphSearchHit> = Vec::new();
        for (name, indices) in &self.name_index {
            if !name.contains(&q) {
                continue;
            }
            let score = if name == &q {
                3.0
            } else if name.ends_with(&q) {
                2.0
            } else {
                1.0
            };
            for &node_index in indices {
                let node = &self.graph[node_index];
                if let Some(filter) = kind_filter
                    && node.kind != filter
                {
                    continue;
                }
                hits.push(RepoGraphSearchHit { node_index, score });
            }
        }
        hits.sort_by(|a, b| {
            b.score.total_cmp(&a.score).then_with(|| {
                let an = &self.graph[a.node_index].display_name;
                let bn = &self.graph[b.node_index].display_name;
                an.len().cmp(&bn.len()).then_with(|| an.cmp(bn))
            })
        });
        hits.truncate(limit);
        hits
    }

    /// Strongly-connected components of size >= `min_size` (defaulting filter
    /// is up to the caller). Trivial single-node SCCs without a self-edge are
    /// always filtered out.
    ///
    /// When `kind_filter` is `Some(File)` or `Some(Symbol)`, the SCC search
    /// runs over the subgraph restricted to that node kind, so mixed
    /// file/symbol strongly-connected components (which the raw graph always
    /// contains because of `ContainsDefinition`/`DeclaredInFile` pairs) do
    /// not mask the cycles we actually care about.
    pub fn strongly_connected_components(
        &self,
        kind_filter: Option<RepoGraphNodeKind>,
        min_size: usize,
    ) -> Vec<Vec<NodeIndex>> {
        use petgraph::visit::NodeFiltered;

        let sccs = if let Some(filter) = kind_filter {
            let filtered = NodeFiltered::from_fn(&self.graph, |n| self.graph[n].kind == filter);
            petgraph::algo::tarjan_scc(&filtered)
        } else {
            petgraph::algo::tarjan_scc(&self.graph)
        };
        sccs.into_iter()
            .filter(|component| {
                if component.len() < min_size {
                    return false;
                }
                if component.len() == 1 {
                    let n = component[0];
                    let has_self_edge = self
                        .graph
                        .edges_directed(n, Outgoing)
                        .any(|e| e.target() == n);
                    if !has_self_edge {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    /// Find orphan nodes (no incoming *reference* edges) optionally filtered
    /// by kind and SCIP visibility. `ContainsDefinition` and `DeclaredInFile`
    /// edges — which are structural "this symbol lives in this file" links,
    /// not uses of the symbol — are not counted as incoming references.
    pub fn orphans(
        &self,
        kind_filter: Option<RepoGraphNodeKind>,
        visibility_filter: Option<ScipVisibility>,
        limit: usize,
    ) -> Vec<NodeIndex> {
        let mut out: Vec<NodeIndex> = Vec::new();
        for node_index in self.graph.node_indices() {
            let node = &self.graph[node_index];
            if node.is_external {
                continue;
            }
            if let Some(filter) = kind_filter
                && node.kind != filter
            {
                continue;
            }
            if let Some(vis) = visibility_filter
                && node.visibility != Some(vis)
            {
                continue;
            }
            let has_incoming_reference =
                self.graph.edges_directed(node_index, Incoming).any(|edge| {
                    !matches!(
                        edge.weight().kind,
                        RepoGraphEdgeKind::ContainsDefinition | RepoGraphEdgeKind::DeclaredInFile
                    )
                });
            if !has_incoming_reference {
                out.push(node_index);
            }
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Returns the [`NodeIndex`]es of symbols whose definition enclosing
    /// range overlaps `[start_line, end_line]` in `file`.
    ///
    /// Lines are 1-indexed inclusive.
    pub fn range_for_node(&self, node: NodeIndex, file: &Path) -> Option<(u32, u32)> {
        let ranges = self.symbol_ranges.get(file)?;
        ranges
            .iter()
            .find(|r| r.node == node)
            .map(|r| (r.start_line, r.end_line))
    }

    pub fn symbols_enclosing(&self, file: &Path, start_line: u32, end_line: u32) -> Vec<NodeIndex> {
        let Some(ranges) = self.symbol_ranges.get(file) else {
            return Vec::new();
        };
        // Ranges can nest (method inside impl inside mod), so a binary search
        // on `start_line` would miss enclosing parents whose start precedes
        // the query window. Linear scan is fine — per-file range counts are
        // small (hundreds at most) and this path is off the hot query loop.
        ranges
            .iter()
            .filter(|range| range.start_line <= end_line && range.end_line >= start_line)
            .map(|range| range.node)
            .collect()
    }

    /// Iterate the per-file symbol-range index in deterministic order.
    /// Each yielded slice is sorted by `start_line` (the invariant
    /// established by [`RepoDependencyGraph::build`]). Used by the chunk-
    /// and-embed pipeline (PR B3) to walk every symbol in every file
    /// without exposing the inner `BTreeMap` shape.
    pub fn symbol_ranges_by_file(&self) -> impl Iterator<Item = (&Path, &[SymbolRange])> {
        self.symbol_ranges
            .iter()
            .map(|(path, ranges)| (path.as_path(), ranges.as_slice()))
    }

    /// PR F3: return the [`crate::communities::Community::id`] for the
    /// community containing `node`, or `None` if `node` is not in any
    /// community (singletons are dropped during detection).
    pub fn community_id(&self, node: NodeIndex) -> Option<&str> {
        let pos = self.community_lookup.get(&node.index())?;
        self.communities.get(*pos).map(|c| c.id.as_str())
    }

    /// Iterate over all detected communities. Empty when community
    /// detection was disabled (`DJINN_COMMUNITY_DETECTION=0`) or when
    /// the graph had no edges. Order matches the on-disk artifact —
    /// largest community first, ties broken by id.
    pub fn communities(&self) -> &[crate::communities::Community] {
        &self.communities
    }

    /// PR F2: every detected [`crate::processes::Process`] in which the
    /// supplied node appears as a step (including processes where the
    /// node is the entry point or the terminal). Returns an empty vec
    /// when the node is not part of any traced flow, when the detector
    /// is disabled, or when the artifact pre-dates v4. The order is
    /// deterministic — sorted by process insertion order, which
    /// follows entry-point discovery order in `detect_processes`.
    pub fn processes_for_node(&self, node: NodeIndex) -> Vec<&crate::processes::Process> {
        let Some(positions) = self.process_lookup.get(&node.index()) else {
            return Vec::new();
        };
        positions
            .iter()
            .filter_map(|&pos| self.processes.get(pos))
            .collect()
    }

    /// Iterate every detected process in deterministic insertion order.
    /// Empty when process detection is disabled or no entry points
    /// produced a flow that survived the pruning rules in
    /// [`crate::processes::detect_processes`].
    pub fn processes(&self) -> &[crate::processes::Process] {
        &self.processes
    }

    /// PR F2: install the detector's output on the graph and rebuild
    /// the reverse `process_lookup` index. Public to crate so
    /// [`crate::processes::detect_processes`] can swap in its result
    /// without exposing a generic mutator surface to outside callers.
    pub(crate) fn set_processes(&mut self, processes: Vec<crate::processes::Process>) {
        self.process_lookup = build_process_lookup(&processes);
        self.processes = processes;
    }

    /// PR F2: stamp a `StepInProcess` edge from a `Process` synthetic
    /// node to a member step. Used internally by
    /// [`crate::processes::detect_processes`].
    pub(crate) fn add_step_in_process_edge(
        &mut self,
        process_node: NodeIndex,
        step_node: NodeIndex,
        step: i32,
    ) {
        let weight = edge_weight_for(RepoGraphEdgeKind::StepInProcess);
        let confidence = edge_confidence_floor(RepoGraphEdgeKind::StepInProcess);
        self.graph.add_edge(
            process_node,
            step_node,
            RepoGraphEdge {
                kind: RepoGraphEdgeKind::StepInProcess,
                weight,
                evidence_count: 1,
                confidence,
                reason: Some("process-step".to_string()),
                step: Some(step),
            },
        );
    }

    /// PR F2: register a new synthetic [`RepoGraphNodeKind::Process`]
    /// node and return its [`NodeIndex`]. Idempotent: returns the
    /// existing index when a process with `id` was already inserted.
    /// Used internally by [`crate::processes::detect_processes`].
    pub(crate) fn ensure_process_node(&mut self, id: &str, label: &str) -> NodeIndex {
        let key = RepoNodeKey::Process(id.to_string());
        if let Some(&idx) = self.node_lookup.get(&key) {
            return idx;
        }
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::Process,
            display_name: label.to_string(),
            language: None,
            file_path: None,
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: None,
        };
        let idx = self.graph.add_node(node);
        self.node_lookup.insert(key, idx);
        idx
    }

    /// Register a synthetic [`RepoGraphNodeKind::Table`] node and
    /// return its [`NodeIndex`]. Idempotent on the lowercased table
    /// name. Used by [`crate::db_access::detect_db_access`].
    pub(crate) fn ensure_table_node(&mut self, name: &str) -> NodeIndex {
        let normalized = name.trim().to_lowercase();
        let key = RepoNodeKey::Table(normalized.clone());
        if let Some(&idx) = self.node_lookup.get(&key) {
            return idx;
        }
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::Table,
            display_name: format!("table:{normalized}"),
            language: None,
            file_path: None,
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: None,
        };
        let idx = self.graph.add_node(node);
        self.node_lookup.insert(key, idx);
        idx
    }

    /// Stamp a `Reads` / `Writes` edge from a caller symbol to a
    /// database-table node. Used by
    /// [`crate::db_access::detect_db_access`] to materialize SQL
    /// access into the canonical graph.
    pub(crate) fn add_table_access_edge(
        &mut self,
        caller: NodeIndex,
        table: NodeIndex,
        kind: RepoGraphEdgeKind,
        reason: &str,
    ) {
        debug_assert!(matches!(
            kind,
            RepoGraphEdgeKind::Reads | RepoGraphEdgeKind::Writes
        ));
        self.graph.add_edge(
            caller,
            table,
            RepoGraphEdge {
                kind,
                weight: edge_weight(kind),
                evidence_count: 1,
                confidence: edge_confidence_floor(kind),
                reason: Some(reason.to_string()),
                step: None,
            },
        );
    }

    /// Shortest dependency path between two nodes using A* over edge weights.
    pub fn shortest_path(
        &self,
        from: NodeIndex,
        to: NodeIndex,
        max_depth: Option<usize>,
    ) -> Option<Vec<NodeIndex>> {
        let result = petgraph::algo::astar(
            &self.graph,
            from,
            |finish| finish == to,
            |edge| edge.weight().weight,
            |_| 0.0,
        );
        let (_cost, nodes) = result?;
        if let Some(max) = max_depth
            && nodes.len().saturating_sub(1) > max
        {
            return None;
        }
        Some(nodes)
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
/// `RepoDependencyGraphBuilder` lives in [`super::builder`] (see
/// `repo_graph/builder.rs`); it was carved out of this module by the
/// follow-up task `3hrr`. The struct is re-exported below for the
/// `build_with_source` / `patch_changed_files` call sites in this
/// module and for the test module.
pub(crate) use self::builder::RepoDependencyGraphBuilder;

impl RepoDependencyGraph {
    /// Replace the community sidecar with a fresh detection pass
    /// result. Rebuilds the reverse `community_lookup` index.
    fn install_communities(&mut self, communities: Vec<crate::communities::Community>) {
        let mut lookup: BTreeMap<usize, usize> = BTreeMap::new();
        for (pos, community) in communities.iter().enumerate() {
            for &node_pos in &community.member_ids {
                lookup.insert(node_pos, pos);
            }
        }
        self.communities = communities;
        self.community_lookup = lookup;
    }
}

fn build_name_index(
    graph: &DiGraph<RepoGraphNode, RepoGraphEdge>,
) -> BTreeMap<String, Vec<NodeIndex>> {
    let mut index: BTreeMap<String, Vec<NodeIndex>> = BTreeMap::new();
    for node_index in graph.node_indices() {
        let node = &graph[node_index];
        let key = node.display_name.to_lowercase();
        index.entry(key).or_default().push(node_index);
    }
    index
}

/// PR F2: build the reverse `node_index → process positions` lookup
/// from a freshly-set process list. The same node can appear in
/// multiple processes (a shared utility called by several entry
/// points), so the value is `Vec<usize>` rather than `Option<usize>`.
fn build_process_lookup(processes: &[crate::processes::Process]) -> BTreeMap<usize, Vec<usize>> {
    let mut out: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (process_pos, process) in processes.iter().enumerate() {
        for step in &process.steps {
            out.entry(step.index()).or_default().push(process_pos);
        }
    }
    out
}

/// True for SCIP symbol kinds whose host is a function declaration in the
/// tree-sitter sense — i.e. `ComplexityWalker::analyze_file` will produce
/// at most one [`crate::complexity::FunctionMetrics`] entry per such
/// symbol when the file's language is supported.
fn is_function_like_symbol_kind(kind: Option<&ScipSymbolKind>) -> bool {
    matches!(
        kind,
        Some(ScipSymbolKind::Function)
            | Some(ScipSymbolKind::Method)
            | Some(ScipSymbolKind::Constructor)
    )
}

/// Iteration 26: attach per-function [`ComplexityMetrics`] to every
/// function-like symbol node in `graph`. Source text is fetched via
/// `load_source(relative_path)`, which is expected to return UTF-8
/// content or `None` (file missing / outside the project root / not
/// UTF-8). Languages unsupported by [`ComplexityWalker`] are silently
/// skipped (the walker returns an empty vec).
///
/// Matching strategy: for every `FunctionMetrics` produced from a file,
/// pick the first function-like graph node in that file whose 1-indexed
/// `SymbolRange` overlaps the walker's 0-indexed `[start_line,
/// end_line]` window. When `name` is set on both sides we prefer a
/// node whose `display_name` matches (the SCIP `display_name` and
/// tree-sitter `name` field can drift slightly across indexers — e.g.
/// `Type::method` vs `method` — so a name match wins outright but its
/// absence is not fatal).
/// Per-function range entry collected while walking a file's symbol nodes:
/// `(node, start_line, end_line, display_name)`.
type FnRangeEntry = (NodeIndex, u32, u32, Option<String>);

fn attach_complexity_metrics<F>(graph: &mut RepoDependencyGraph, mut load_source: F)
where
    F: FnMut(&Path) -> Option<String>,
{
    // Collect candidate files first: any file with at least one function-
    // like symbol node and a non-empty `language`. The symbol_ranges
    // sidecar already keys on PathBuf and gives us 1-indexed inclusive
    // ranges per node, so we use it as the iteration root.
    let candidates: Vec<(PathBuf, String, Vec<FnRangeEntry>)> = graph
        .symbol_ranges_by_file()
        .filter_map(|(path, ranges)| {
            // Take the first function-like node we find in this file just
            // to read the language hint (every node in a file shares the
            // SCIP `Document.language`, so any one works). Skip files
            // without a function-like node — nothing to compute.
            let mut entries: Vec<FnRangeEntry> = Vec::new();
            let mut language: Option<String> = None;
            for range in ranges {
                let node = graph.node(range.node);
                if !is_function_like_symbol_kind(node.symbol_kind.as_ref()) {
                    continue;
                }
                if language.is_none() {
                    language = node.language.clone();
                }
                entries.push((
                    range.node,
                    range.start_line,
                    range.end_line,
                    Some(node.display_name.clone()),
                ));
            }
            let lang = language?;
            if entries.is_empty() {
                return None;
            }
            Some((path.to_path_buf(), lang, entries))
        })
        .collect();

    if candidates.is_empty() {
        return;
    }

    let mut walker = ComplexityWalker::new();
    for (rel_path, language, mut nodes) in candidates {
        let Some(source) = load_source(&rel_path) else {
            continue;
        };
        let metrics = walker.analyze_file(&language, &source);
        if metrics.is_empty() {
            continue;
        }
        // Track which node indices we have already populated so two
        // FunctionMetrics whose ranges overlap the same SCIP enclosing
        // range don't fight over it.
        let mut consumed: BTreeSet<NodeIndex> = BTreeSet::new();
        for fm in metrics {
            // SCIP ranges are 1-indexed inclusive (see record_symbol_range);
            // walker ranges are 0-indexed, end-line inclusive on the
            // declaration's last line. Bring both into the SCIP frame.
            let fm_start = fm.start_line.saturating_add(1);
            let fm_end = fm.end_line.saturating_add(1);

            // Overlap = SCIP[start..=end] ∩ walker[start..=end] non-empty.
            let mut name_hit: Option<usize> = None;
            let mut overlap_hit: Option<usize> = None;
            for (i, (node_idx, scip_start, scip_end, display_name)) in nodes.iter().enumerate() {
                if consumed.contains(node_idx) {
                    continue;
                }
                let overlaps = *scip_start <= fm_end && *scip_end >= fm_start;
                if !overlaps {
                    continue;
                }
                if name_hit.is_none()
                    && let (Some(disp), Some(fn_name)) =
                        (display_name.as_deref(), fm.name.as_deref())
                    && names_match(disp, fn_name)
                {
                    name_hit = Some(i);
                }
                if overlap_hit.is_none() {
                    overlap_hit = Some(i);
                }
            }
            let chosen = name_hit.or(overlap_hit);
            let Some(idx_in_nodes) = chosen else {
                continue;
            };
            let node_idx = nodes[idx_in_nodes].0;
            consumed.insert(node_idx);
            graph.graph_mut_unchecked()[node_idx].complexity = Some(fm.metrics);
        }
        // Drop bookkeeping for this file — keeps memory flat across large
        // candidate sets.
        nodes.clear();
    }
}

/// Loose name-match between a SCIP `display_name` and a tree-sitter
/// `name` field. SCIP indexers occasionally prefix the receiver type
/// (`Foo::bar`, `Foo.bar`), while tree-sitter only sees the bare
/// identifier — accept either when the suffix lines up.
fn names_match(scip_display: &str, ts_name: &str) -> bool {
    if scip_display == ts_name {
        return true;
    }
    if let Some((_, tail)) = scip_display.rsplit_once("::")
        && tail == ts_name
    {
        return true;
    }
    if let Some((_, tail)) = scip_display.rsplit_once('.')
        && tail == ts_name
    {
        return true;
    }
    false
}

/// Minimal serializable artifact capturing the per-file and per-symbol graph
/// relationships needed for incremental changed-file patch planning.
///
/// This is persisted alongside the rendered repo-map cache so that later
/// operations can recover the dependency graph without re-parsing raw SCIP
/// outputs.
///
/// The `version` field is mandatory in PR A2+. Old blobs that pre-date this
/// field will fail to bincode-deserialize (positional encoding) and trigger
/// a re-warm via the `load_cached_artifact` "stale or unreadable" branch in
/// `canonical_graph.rs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoGraphArtifact {
    /// Schema version stamp. See [`REPO_GRAPH_ARTIFACT_VERSION`] for the
    /// current value and the bump history.
    pub version: u32,
    pub nodes: Vec<RepoGraphNode>,
    pub edges: Vec<RepoGraphArtifactEdge>,
    /// Per-file enclosing-range sidecar, keyed by file path. Each range refers
    /// to a node by its position in `nodes`. Persisting this here is what
    /// keeps `symbols_enclosing` non-empty after a cache-hit reload.
    #[serde(default)]
    pub symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
    /// PR F3 community sidecar. Each community references its members
    /// by their position in [`Self::nodes`]. New in artifact v4 — old
    /// blobs that lack the field deserialize via `default` to an
    /// empty vec, which is fine: `community_id(...)` then returns
    /// `None` until the next warm rebuild repopulates it.
    #[serde(default)]
    pub communities: Vec<crate::communities::Community>,
    /// PR F2: detected execution-flow processes. Each `Process` carries
    /// node-position references into `nodes`; persisting it here lets
    /// `processes_for_node` answer queries after a cache-hit reload
    /// without re-running the detector.
    #[serde(default)]
    pub processes: Vec<RepoGraphArtifactProcess>,
}

#[derive(Debug, Deserialize)]
struct RepoGraphArtifactV10WithoutWorkspace {
    version: u32,
    nodes: Vec<RepoGraphNodeV10WithoutWorkspace>,
    edges: Vec<RepoGraphArtifactEdge>,
    #[serde(default)]
    symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
    #[serde(default)]
    communities: Vec<crate::communities::Community>,
    #[serde(default)]
    processes: Vec<RepoGraphArtifactProcess>,
}

#[derive(Debug, Deserialize)]
struct RepoGraphNodeV10WithoutWorkspace {
    id: RepoNodeKey,
    kind: RepoGraphNodeKind,
    display_name: String,
    language: Option<String>,
    file_path: Option<PathBuf>,
    symbol: Option<String>,
    symbol_kind: Option<ScipSymbolKind>,
    is_external: bool,
    #[serde(default)]
    visibility: Option<ScipVisibility>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    documentation: Vec<String>,
    #[serde(default)]
    signature_parts: Option<crate::scip_parser::ScipSignatureParts>,
    #[serde(default)]
    is_test: bool,
    #[serde(default)]
    complexity: Option<ComplexityMetrics>,
}

impl From<RepoGraphArtifactV10WithoutWorkspace> for RepoGraphArtifact {
    fn from(old: RepoGraphArtifactV10WithoutWorkspace) -> Self {
        Self {
            version: old.version,
            nodes: old.nodes.into_iter().map(RepoGraphNode::from).collect(),
            edges: old.edges,
            symbol_ranges: old.symbol_ranges,
            communities: old.communities,
            processes: old.processes,
        }
    }
}

impl From<RepoGraphNodeV10WithoutWorkspace> for RepoGraphNode {
    fn from(old: RepoGraphNodeV10WithoutWorkspace) -> Self {
        Self {
            id: old.id,
            kind: old.kind,
            display_name: old.display_name,
            language: old.language,
            file_path: old.file_path,
            symbol: old.symbol,
            symbol_kind: old.symbol_kind,
            is_external: old.is_external,
            visibility: old.visibility,
            signature: old.signature,
            documentation: old.documentation,
            signature_parts: old.signature_parts,
            is_test: old.is_test,
            complexity: old.complexity,
            workspace: None,
        }
    }
}

/// Deserialize a repo-graph bincode artifact, accepting both the current v10
/// node layout (with `workspace`) and pre-workspace v10 blobs that do not carry
/// the appended node field. The artifact version deliberately remains v10, so
/// callers on the persisted cache path must use this compatibility seam instead
/// of raw `bincode::deserialize`.
pub fn deserialize_repo_graph_artifact_bincode(blob: &[u8]) -> Result<RepoGraphArtifact, String> {
    match bincode::deserialize::<RepoGraphArtifact>(blob) {
        Ok(artifact) => Ok(artifact),
        Err(current_err) => bincode::deserialize::<RepoGraphArtifactV10WithoutWorkspace>(blob)
            .map(RepoGraphArtifact::from)
            .map_err(|compat_err| {
                format!(
                    "deserialize graph: {current_err}; v10 pre-workspace fallback also failed: {compat_err}"
                )
            }),
    }
}

/// A serializable directed edge between two graph nodes, identified by their
/// position in the `nodes` vec of the parent [`RepoGraphArtifact`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoGraphArtifactEdge {
    pub source: usize,
    pub target: usize,
    pub kind: RepoGraphEdgeKind,
    pub weight: f64,
    pub evidence_count: usize,
    /// Edge confidence in [0, 1]; mirrors [`RepoGraphEdge::confidence`].
    /// New in artifact v1 (PR A2).
    pub confidence: f64,
    /// Optional reason explaining the confidence value; mirrors
    /// [`RepoGraphEdge::reason`]. New in artifact v1 (PR A2).
    pub reason: Option<String>,
    /// PR F2: 0-indexed step ordinal for [`RepoGraphEdgeKind::StepInProcess`]
    /// edges. `None` for every other kind. New in artifact v4 (PR F2).
    #[serde(default)]
    pub step: Option<i32>,
}

/// A serializable enclosing range for a symbol definition, identified by the
/// symbol node's position in the parent [`RepoGraphArtifact::nodes`] vec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGraphArtifactSymbolRange {
    pub start_line: u32,
    pub end_line: u32,
    pub node: usize,
}

/// PR F2: serializable form of [`crate::processes::Process`] keyed by
/// node positions in the parent [`RepoGraphArtifact::nodes`] vec
/// rather than by `NodeIndex` (which is not stable across artifact
/// rebuilds). New in artifact v4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGraphArtifactProcess {
    /// Stable process id — sha256 of the entry-point uid + step count,
    /// truncated to 16 hex chars. Mirrors [`crate::processes::Process::id`].
    pub id: String,
    /// Human-readable label (entry point's display name + " process").
    pub label: String,
    /// Position in `nodes` of the synthetic [`RepoGraphNodeKind::Process`]
    /// node materialized for this flow.
    pub process_node: usize,
    /// Position in `nodes` of the entry-point symbol that originated
    /// this flow.
    pub entry_point: usize,
    /// Position in `nodes` of the last node along the trace.
    pub terminal: usize,
    /// Ordered node positions along the trace, including the entry
    /// point at `[0]` and the terminal at `[step_count - 1]`.
    pub steps: Vec<usize>,
}

impl RepoDependencyGraph {
    /// Serialize the graph into a compact JSON artifact suitable for DB
    /// persistence.
    pub fn to_artifact(&self) -> RepoGraphArtifact {
        let mut index_map: BTreeMap<NodeIndex, usize> = BTreeMap::new();
        let mut nodes = Vec::with_capacity(self.graph.node_count());
        for (i, node_index) in self.graph.node_indices().enumerate() {
            index_map.insert(node_index, i);
            nodes.push(self.graph[node_index].clone());
        }

        let mut edges = Vec::with_capacity(self.graph.edge_count());
        for edge_ref in self.graph.edge_references() {
            let source = index_map[&edge_ref.source()];
            let target = index_map[&edge_ref.target()];
            let w = edge_ref.weight();
            edges.push(RepoGraphArtifactEdge {
                source,
                target,
                kind: w.kind,
                weight: w.weight,
                evidence_count: w.evidence_count,
                confidence: w.confidence,
                reason: w.reason.clone(),
                step: w.step,
            });
        }

        let mut symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>> =
            BTreeMap::new();
        for (file, ranges) in &self.symbol_ranges {
            let mut translated = Vec::with_capacity(ranges.len());
            for range in ranges {
                // Skip ranges whose node isn't in the artifact's node table —
                // shouldn't happen in practice, but guards against bookkeeping
                // drift between the petgraph and the sidecar.
                let Some(&node_pos) = index_map.get(&range.node) else {
                    continue;
                };
                translated.push(RepoGraphArtifactSymbolRange {
                    start_line: range.start_line,
                    end_line: range.end_line,
                    node: node_pos,
                });
            }
            if !translated.is_empty() {
                symbol_ranges.insert(file.clone(), translated);
            }
        }

        // PR F2: serialize the process sidecar. Each `Process` is keyed
        // by node positions (a `Vec<usize>`) rather than `NodeIndex`
        // values so the artifact survives a `from_artifact` rebuild.
        let mut processes_out: Vec<RepoGraphArtifactProcess> =
            Vec::with_capacity(self.processes.len());
        for process in &self.processes {
            let Some(&entry_pos) = index_map.get(&process.entry_point_id) else {
                continue;
            };
            let Some(&terminal_pos) = index_map.get(&process.terminal_id) else {
                continue;
            };
            let Some(&process_node_pos) = index_map.get(&process.process_node_id) else {
                continue;
            };
            let mut steps_out = Vec::with_capacity(process.steps.len());
            let mut steps_complete = true;
            for step in &process.steps {
                let Some(&pos) = index_map.get(step) else {
                    steps_complete = false;
                    break;
                };
                steps_out.push(pos);
            }
            if !steps_complete {
                continue;
            }
            processes_out.push(RepoGraphArtifactProcess {
                id: process.id.clone(),
                label: process.label.clone(),
                process_node: process_node_pos,
                entry_point: entry_pos,
                terminal: terminal_pos,
                steps: steps_out,
            });
        }

        RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges,
            communities: self.communities.clone(),
            processes: processes_out,
        }
    }

    /// Rebuild a `RepoDependencyGraph` from a previously persisted artifact.
    pub fn from_artifact(artifact: &RepoGraphArtifact) -> Self {
        let mut graph = DiGraph::new();
        let mut node_lookup = BTreeMap::new();
        let mut index_map = Vec::with_capacity(artifact.nodes.len());

        for node in &artifact.nodes {
            let node_index = graph.add_node(node.clone());
            node_lookup.insert(node.id.clone(), node_index);
            index_map.push(node_index);
        }

        for edge in &artifact.edges {
            graph.add_edge(
                index_map[edge.source],
                index_map[edge.target],
                RepoGraphEdge {
                    kind: edge.kind,
                    weight: edge.weight,
                    evidence_count: edge.evidence_count,
                    confidence: edge.confidence,
                    reason: edge.reason.clone(),
                    step: edge.step,
                },
            );
        }

        let name_index = build_name_index(&graph);

        let mut symbol_ranges: BTreeMap<PathBuf, Vec<SymbolRange>> = BTreeMap::new();
        for (file, ranges) in &artifact.symbol_ranges {
            let mut translated = Vec::with_capacity(ranges.len());
            for range in ranges {
                let Some(&node) = index_map.get(range.node) else {
                    continue;
                };
                translated.push(SymbolRange {
                    start_line: range.start_line,
                    end_line: range.end_line,
                    node,
                });
            }
            translated.sort_by_key(|r| (r.start_line, r.end_line));
            if !translated.is_empty() {
                symbol_ranges.insert(file.clone(), translated);
            }
        }

        // PR F2: rehydrate the process sidecar. Reject any process whose
        // step list references a node position outside the artifact's
        // bounds — defensive guard against an artifact and node table
        // that drifted out of sync.
        let mut processes: Vec<crate::processes::Process> =
            Vec::with_capacity(artifact.processes.len());
        for process in &artifact.processes {
            let Some(&entry_id) = index_map.get(process.entry_point) else {
                continue;
            };
            let Some(&terminal_id) = index_map.get(process.terminal) else {
                continue;
            };
            let Some(&process_node_id) = index_map.get(process.process_node) else {
                continue;
            };
            let mut steps_out = Vec::with_capacity(process.steps.len());
            let mut steps_complete = true;
            for &step_pos in &process.steps {
                let Some(&node) = index_map.get(step_pos) else {
                    steps_complete = false;
                    break;
                };
                steps_out.push(node);
            }
            if !steps_complete {
                continue;
            }
            processes.push(crate::processes::Process {
                id: process.id.clone(),
                label: process.label.clone(),
                process_node_id,
                entry_point_id: entry_id,
                terminal_id,
                step_count: steps_out.len(),
                steps: steps_out,
            });
        }
        let process_lookup = build_process_lookup(&processes);

        let mut out = RepoDependencyGraph {
            graph,
            node_lookup,
            name_index,
            symbol_ranges,
            communities: Vec::new(),
            community_lookup: BTreeMap::new(),
            processes,
            process_lookup,
        };
        // PR F3: rehydrate the community sidecar verbatim — node
        // positions in the artifact match `NodeIndex` 0..n thanks to the
        // ordered `add_node` loop above.
        if !artifact.communities.is_empty() {
            out.install_communities(artifact.communities.clone());
        }
        out
    }

    /// Serialize the graph artifact to a JSON string for DB storage.
    pub fn serialize_artifact(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.to_artifact())
    }

    /// Deserialize a graph from a previously stored JSON artifact string.
    #[cfg(test)]
    pub fn deserialize_artifact(json: &str) -> Result<Self, serde_json::Error> {
        let artifact: RepoGraphArtifact = serde_json::from_str(json)?;
        Ok(Self::from_artifact(&artifact))
    }

    /// Patch the graph by removing all contributions from `changed_files` and
    /// re-adding them from the supplied SCIP parse output.
    ///
    /// This is the core of the small-diff incremental path: instead of
    /// rebuilding the entire graph from scratch we strip the stale file/symbol
    /// nodes and edges, then replay only the changed files through the normal
    /// builder pipeline.
    ///
    /// The caller is responsible for ensuring `new_indices` contains parsed
    /// SCIP data for exactly the changed files (additional files are harmless
    /// but defeat the purpose).
    #[cfg(test)]
    pub fn patch_changed_files(
        &self,
        changed_files: &BTreeSet<PathBuf>,
        new_indices: &[ParsedScipIndex],
    ) -> Self {
        // Step 1: Build a filtered artifact that excludes nodes owned by
        // changed files and any edges touching those nodes.
        let artifact = self.to_artifact();
        let removed_positions: BTreeSet<usize> = artifact
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| is_owned_by_changed_file(node, changed_files))
            .map(|(i, _)| i)
            .collect();

        // Collect surviving nodes and build old-position -> new-position map.
        let mut position_map: BTreeMap<usize, usize> = BTreeMap::new();
        let mut surviving_nodes = Vec::new();
        for (old_pos, node) in artifact.nodes.iter().enumerate() {
            if removed_positions.contains(&old_pos) {
                continue;
            }
            position_map.insert(old_pos, surviving_nodes.len());
            surviving_nodes.push(node.clone());
        }

        let surviving_edges: Vec<RepoGraphArtifactEdge> = artifact
            .edges
            .iter()
            .filter(|edge| {
                !removed_positions.contains(&edge.source)
                    && !removed_positions.contains(&edge.target)
            })
            .map(|edge| RepoGraphArtifactEdge {
                source: position_map[&edge.source],
                target: position_map[&edge.target],
                kind: edge.kind,
                weight: edge.weight,
                evidence_count: edge.evidence_count,
                confidence: edge.confidence,
                reason: edge.reason.clone(),
                step: edge.step,
            })
            .collect();

        let mut surviving_symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>> =
            BTreeMap::new();
        for (file, ranges) in &artifact.symbol_ranges {
            if changed_files.contains(file) {
                continue;
            }
            let mut translated = Vec::with_capacity(ranges.len());
            for range in ranges {
                let Some(&new_node) = position_map.get(&range.node) else {
                    continue;
                };
                translated.push(RepoGraphArtifactSymbolRange {
                    start_line: range.start_line,
                    end_line: range.end_line,
                    node: new_node,
                });
            }
            if !translated.is_empty() {
                surviving_symbol_ranges.insert(file.clone(), translated);
            }
        }

        // PR F2: drop the process sidecar entirely on patch — the
        // changed files may have rewritten the call chains the trace
        // followed, and the test path doesn't exercise the process
        // detector anyway. The next full rebuild re-runs detection
        // from scratch.
        let filtered_artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: surviving_nodes,
            edges: surviving_edges,
            symbol_ranges: surviving_symbol_ranges,
            // Communities are recomputed when the rebuilt graph runs
            // through `finish()`; dropping the stale sidecar here is
            // the safe choice since member positions get remapped
            // anyway.
            communities: Vec::new(),
            // Processes are likewise recomputed by the post-build pass.
            processes: Vec::new(),
        };

        // Step 2: Rebuild the base graph from the filtered artifact.
        // We use a builder so that the new SCIP data can link to existing
        // nodes (e.g. symbols defined in unchanged files that are referenced
        // by changed files).
        let base = Self::from_artifact(&filtered_artifact);
        let mut builder = RepoDependencyGraphBuilder {
            graph: base.graph,
            node_lookup: base.node_lookup,
            symbol_ranges: base.symbol_ranges,
            ..Default::default()
        };
        // Reconstruct declared_symbols and symbol_file from the surviving nodes.
        for node_index in builder.graph.node_indices() {
            let node = &builder.graph[node_index];
            if let RepoGraphNodeKind::Symbol = node.kind
                && let Some(sym) = &node.symbol
            {
                if !node.is_external {
                    builder.declared_symbols.insert(sym.clone());
                }
                if let Some(fp) = &node.file_path {
                    builder.symbol_file.insert(sym.clone(), fp.clone());
                }
                if let Some(lang) = &node.language {
                    builder.symbol_language.insert(sym.clone(), lang.clone());
                }
            }
        }

        // Step 3: Replay changed-file SCIP data through the builder.
        for index in new_indices {
            for file in &index.files {
                if changed_files.contains(&file.relative_path) {
                    builder.add_file(file);
                }
            }
        }

        builder.finish()
    }
}

/// Returns `true` when `node` is "owned by" one of the changed files:
/// - file nodes whose path is in the set
/// - symbol nodes whose `file_path` is in the set *and* that are not external
#[cfg(test)]
fn is_owned_by_changed_file(node: &RepoGraphNode, changed_files: &BTreeSet<PathBuf>) -> bool {
    match &node.kind {
        RepoGraphNodeKind::File => node
            .file_path
            .as_ref()
            .is_some_and(|p| changed_files.contains(p)),
        RepoGraphNodeKind::Symbol => {
            !node.is_external
                && node
                    .file_path
                    .as_ref()
                    .is_some_and(|p| changed_files.contains(p))
        }
        // PR F2: synthetic process nodes are never owned by a changed
        // file — `patch_changed_files` always drops the process
        // sidecar entirely (see the filtered-artifact construction
        // above) and lets the next full rebuild re-trace.
        RepoGraphNodeKind::Process => false,
        // Synthetic table nodes — same: they're rebuilt by the
        // db-access pass on the next warm.
        RepoGraphNodeKind::Table => false,
    }
}
/// True when the node represents a SCIP symbol whose identifier is
/// document-local (`local …`). File nodes and globally-scoped symbols
/// return `false`.
fn node_is_local_symbol(node: &RepoGraphNode) -> bool {
    if !matches!(node.kind, RepoGraphNodeKind::Symbol) {
        return false;
    }
    matches!(node.visibility, Some(ScipVisibility::Private))
        || node
            .symbol
            .as_deref()
            .is_some_and(|s| s.starts_with("local "))
}

/// Compute the confidence/reason pair for a freshly-built edge.
///
/// Starts from the per-kind floor (see [`edge_confidence_floor`]). When
/// either the source or target node is a `local`-prefixed symbol, lowers
/// the confidence by [`EDGE_CONFIDENCE_LOCAL_PENALTY`] and stamps the
/// edge with `reason="local-prefix"` so callers can tell why the value
/// dropped.
fn derive_edge_confidence(
    graph: &DiGraph<RepoGraphNode, RepoGraphEdge>,
    source: NodeIndex,
    target: NodeIndex,
    kind: RepoGraphEdgeKind,
) -> (f64, Option<String>) {
    let mut confidence = edge_confidence_floor(kind);
    let mut reason: Option<String> = None;

    let source_local = node_is_local_symbol(&graph[source]);
    let target_local = node_is_local_symbol(&graph[target]);
    if source_local || target_local {
        confidence = (confidence - EDGE_CONFIDENCE_LOCAL_PENALTY).clamp(0.0, 1.0);
        reason = Some("local-prefix".to_string());
    }

    (confidence, reason)
}
