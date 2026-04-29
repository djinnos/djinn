use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::Direction::{Incoming, Outgoing};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef as PetgraphEdgeRef;
use serde::{Deserialize, Serialize};

use crate::complexity::{ComplexityMetrics, ComplexityWalker};
use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipOccurrence, ScipRelationship, ScipRelationshipKind, ScipSymbol,
    ScipSymbolKind, ScipSymbolRole, ScipVisibility,
};

const PAGE_RANK_DAMPING_FACTOR: f64 = 0.85;
const PAGE_RANK_ITERATIONS: usize = 25;

/// Repo-graph artifact schema version.
///
/// Bumped when the on-disk shape (struct fields, enum variants) changes in
/// ways that would silently corrupt a bincode load. Old blobs that do not
/// carry this field — or carry a lower value — are rejected by
/// [`RepoDependencyGraph::from_artifact`] (via bincode failure on the new
/// field set) and force a re-warm.
///
/// Bump history:
/// - v1 (initial): added `confidence` and `reason` to every edge (PR A2).
/// - v2: split `SymbolReference` into `Reads` / `Writes` based on SCIP
///   `SymbolRole::ReadAccess` / `WriteAccess` flags (PR A3). Old blobs
///   bincode-deserialize but their edges are stamped with the legacy
///   `SymbolReference` kind only — next warm rebuilds with the split.
/// - v3: entry-point detection (PR F1) — adds `EntryPointOf` edge kind
///   and `is_test` flag on `RepoGraphNode`. Old v2 blobs bincode-fail on
///   the new edge variant / extra node field and trigger a re-warm.
/// - v4: persist the [`crate::communities::Community`] sidecar in the
///   artifact and add the [`RepoGraphEdgeKind::MemberOf`] variant
///   (PR F3). Old blobs bincode-deserialize with empty communities;
///   next warm runs greedy modularity detection and populates them.
/// - v5: process (execution flow) detection (PR F2) — adds
///   `RepoGraphEdgeKind::StepInProcess`, `RepoGraphNodeKind::Process`,
///   `RepoNodeKey::Process(String)`, an optional `step` ordinal on each
///   edge, and a `processes: Vec<Process>` sidecar on the artifact. Old
///   v4 blobs bincode-fail on the new variants / extra fields and force
///   a re-warm.
/// - v6: rename the four `SymbolRelationship*` edge variants to their
///   semantic names (`Extends`, `Implements`, `TypeDefines`, `Defines`).
///   The on-wire bincode positional encoding is unchanged (variant
///   order preserved), but the serde rename surface and public JSON
///   field names shift, so the version stamp is bumped to communicate
///   the public-API break to any consumers parsing serialized output.
/// - v7: DB-access detection — adds `RepoGraphNodeKind::Table` and
///   `RepoNodeKey::Table(String)` for synthetic database-table nodes,
///   plus `Reads`/`Writes` edges from caller symbols to table nodes.
///   Old v6 blobs bincode-fail on the new `RepoNodeKey` / kind
///   variants and trigger a re-warm.
/// - v8: drop function/method-scoped `Variable`/`Parameter` SCIP symbols at
///   parse time to avoid super-nodes (every `ctx`/`err`/`logger` across the
///   repo collapsing into one). Old v7 blobs still bincode-deserialize but
///   contain the polluted node set; the version bump forces a re-warm so
///   the on-disk cache reflects the cleaner graph. Filter predicate:
///   see `crate::scip_parser::is_function_scoped_variable`.
/// - v9: per-function complexity metrics (iteration 26) — adds
///   `complexity: Option<ComplexityMetrics>` to every `RepoGraphNode`.
///   Populated for function-like symbols whose host file the
///   [`ComplexityWalker`] can parse (currently Rust; more languages
///   land in iter 24/25). Old v8 blobs do not carry the field;
///   `#[serde(default)]` lets them deserialize as `None`, but the
///   version bump still forces a re-warm so caches reflect the
///   freshly-computed metrics rather than running indefinitely with
///   `None` everywhere.
pub const REPO_GRAPH_ARTIFACT_VERSION: u32 = 9;

// ── Edge confidence floor table (PR A2) ────────────────────────────────────
//
// Initial confidence assigned to every edge of a given kind. The visibility
// heuristic (a `local `-prefixed source or target symbol) lowers the floor by
// `EDGE_CONFIDENCE_LOCAL_PENALTY` and stamps `reason="local-prefix"` on the
// edge so downstream filters can explain themselves.

const EDGE_CONFIDENCE_CONTAINS_DEFINITION: f64 = 0.95;
const EDGE_CONFIDENCE_DECLARED_IN_FILE: f64 = 0.95;
const EDGE_CONFIDENCE_FILE_REFERENCE: f64 = 0.85;
const EDGE_CONFIDENCE_SYMBOL_REFERENCE: f64 = 0.90;
const EDGE_CONFIDENCE_EXTENDS: f64 = 0.80;
const EDGE_CONFIDENCE_IMPLEMENTS: f64 = 0.85;
const EDGE_CONFIDENCE_TYPE_DEFINES: f64 = 0.85;
const EDGE_CONFIDENCE_DEFINES: f64 = 0.85;
// PR A3: split confidences for `Reads` / `Writes` (carved out of
// `SymbolReference`). Writes are the more reliable signal because SCIP's
// `WriteAccess` flag is set deterministically by the indexer at the
// assignment site; reads cover both load/use sites and method-call
// receivers, so they sit slightly lower. The plan didn't pin numbers —
// we use 0.90 / 0.85 so `Writes` matches the old `SymbolReference`
// floor (no regression for write-detection downstream) and `Reads` takes
// a one-tier penalty.
const EDGE_CONFIDENCE_READS: f64 = 0.85;
const EDGE_CONFIDENCE_WRITES: f64 = 0.90;
// PR F1: floor for `EntryPointOf` edges. The detector itself records
// per-hit confidence in [0.6, 0.95] depending on signal strength
// (`fn main`, SCIP `Test` role → 0.95; file-path heuristics → 0.7;
// import-shape heuristics → 0.6). The floor only matters when the edge
// is added with a confidence below 0.5 — we set it to 0.5 so the table
// stays consistent with the rest of the file. Per-hit confidences
// override the floor in [`detect_entry_points`].
const EDGE_CONFIDENCE_ENTRY_POINT_OF: f64 = 0.5;
// PR F3: synthesized `Community` membership edge — confidence floor
// 0.95 since the modularity partition is deterministic for a given
// graph. Same tier as `ContainsDefinition` / `DeclaredInFile` (also
// algorithmically derived from SCIP, not sampled).
const EDGE_CONFIDENCE_MEMBER_OF: f64 = 0.95;
// PR F2: `StepInProcess` edges are synthetic links from a `Process`
// node to each step in the deterministic call chain it traces. They
// carry the same 0.95 floor as `ContainsDefinition` / `DeclaredInFile`
// — the partition is computed from the SCIP-derived edge structure, so
// every `StepInProcess` is as trustworthy as the strongest source edge
// the trace consumed.
const EDGE_CONFIDENCE_STEP_IN_PROCESS: f64 = 0.95;
const EDGE_CONFIDENCE_LOCAL_PENALTY: f64 = 0.15;
const EDGE_WEIGHT_DEFINITION_TO_FILE: f64 = 4.0;
const EDGE_WEIGHT_FILE_TO_DEFINITION: f64 = 1.5;
const EDGE_WEIGHT_FILE_REFERENCE: f64 = 2.5;
const EDGE_WEIGHT_SYMBOL_REFERENCE: f64 = 3.5;
const EDGE_WEIGHT_EXTENDS: f64 = 2.0;
const EDGE_WEIGHT_IMPLEMENTS: f64 = 2.5;
const EDGE_WEIGHT_TYPE_DEFINES: f64 = 1.75;
const EDGE_WEIGHT_DEFINES: f64 = 2.25;
// PR F1: keep `EntryPointOf` light — the edge is metadata, not a
// dependency signal, so it should not perturb PageRank or shortest-path
// scoring.
const EDGE_WEIGHT_ENTRY_POINT_OF: f64 = 0.5;
// PR F3: `MemberOf` edges are structural (not weighted by SCIP
// evidence count), so they get a constant low weight that doesn't
// dominate PageRank. The community is a side-channel; it shouldn't
// reshape the importance ranking.
const EDGE_WEIGHT_MEMBER_OF: f64 = 1.0;
// PR F2: `StepInProcess` edges are structural metadata (not new SCIP
// evidence), so they get a constant low weight that does not dominate
// PageRank or A* shortest-path queries. Process nodes are a side-
// channel: they should not reshape the importance ranking of the
// underlying call graph.
const EDGE_WEIGHT_STEP_IN_PROCESS: f64 = 0.5;
const SYMBOL_KIND_TYPE_MULTIPLIER: f64 = 1.15;
const SYMBOL_KIND_METHOD_MULTIPLIER: f64 = 1.05;
const SYMBOL_KIND_FUNCTION_MULTIPLIER: f64 = 1.0;
const SYMBOL_KIND_VARIABLE_MULTIPLIER: f64 = 0.7;
const SYMBOL_KIND_DEFAULT_MULTIPLIER: f64 = 0.9;

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
            if !distances.contains_key(&target) {
                distances.insert(target, next_dist);
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
            (Some(x), Some(y)) => x
                .cmp(&y)
                .then_with(|| nodes[a].key.cmp(&nodes[b].key)),
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
    pub fn build_with_source(
        indices: &[ParsedScipIndex],
        project_root: Option<&Path>,
    ) -> Self {
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
    pub(crate) fn graph_mut_unchecked(
        &mut self,
    ) -> &mut DiGraph<RepoGraphNode, RepoGraphEdge> {
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

    pub fn symbols_enclosing(
        &self,
        file: &Path,
        start_line: u32,
        end_line: u32,
    ) -> Vec<NodeIndex> {
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

/// Internal hit returned by `search_by_name`. The bridge converts this to the
/// public `SearchHit` data type.
#[derive(Debug, Clone)]
pub struct RepoGraphSearchHit {
    pub node_index: NodeIndex,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RepoNodeKey {
    File(PathBuf),
    Symbol(String),
    /// PR F2: synthetic node identifying a deterministic execution
    /// flow traced from an entry point. The string is the stable
    /// process id (sha256 of `entry_point_uid || step_count` truncated
    /// to 16 hex chars) — see [`crate::processes::Process::id`].
    Process(String),
    /// Synthetic node identifying a database table referenced by raw
    /// SQL or ORM access in source code. The string is the lowercased,
    /// schema-qualified table name (`"public.users"`, or just `"users"`
    /// when no schema is present). Materialized by
    /// [`crate::db_access::detect_db_access`]; receivers of `Reads` /
    /// `Writes` edges from the enclosing function/method symbol.
    /// Kept under the same enum so name-index / search / impact ops
    /// surface tables transparently alongside symbols.
    Table(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGraphNodeKind {
    File,
    Symbol,
    /// PR F2: synthetic execution-flow node materialized by
    /// [`crate::processes::detect_processes`]. Carries no SCIP-derived
    /// metadata of its own (no `file_path`, no `symbol_kind`); the
    /// node's identity lives entirely in [`RepoNodeKey::Process`]. Hung
    /// off the canonical graph by a chain of `StepInProcess` edges.
    Process,
    /// Synthetic database-table node materialized by
    /// [`crate::db_access::detect_db_access`]. Identity in
    /// [`RepoNodeKey::Table`]; carries `display_name` only. Receives
    /// `Reads` / `Writes` edges from enclosing function symbols whose
    /// bodies contain raw SQL touching the table.
    Table,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGraphNode {
    pub id: RepoNodeKey,
    pub kind: RepoGraphNodeKind,
    pub display_name: String,
    pub language: Option<String>,
    pub file_path: Option<PathBuf>,
    pub symbol: Option<String>,
    pub symbol_kind: Option<ScipSymbolKind>,
    pub is_external: bool,
    /// Visibility of the underlying SCIP symbol, when known. `None` for file
    /// nodes and for synthetic placeholder symbols.
    #[serde(default)]
    pub visibility: Option<ScipVisibility>,
    /// Symbol signature, copied from `ScipSymbol::signature` when present.
    #[serde(default)]
    pub signature: Option<String>,
    /// Symbol documentation, copied from `ScipSymbol::documentation`.
    #[serde(default)]
    pub documentation: Vec<String>,
    /// PR C1: structured signature parts when SCIP populates them.
    /// Propagated from `ScipSymbol::signature_parts`. `None` for indexers
    /// that emit only the markdown signature blob — `code_graph context`
    /// surfaces this as `method_metadata: None` rather than regexing.
    #[serde(default)]
    pub signature_parts: Option<crate::scip_parser::ScipSignatureParts>,
    /// PR F1: SCIP-derived test marker. `true` when at least one of the
    /// symbol's definition occurrences carries the SCIP `Test` role
    /// (`SymbolRole::Test`, bit 32). Used by
    /// [`crate::entry_points::detect_entry_points`] as the high-confidence
    /// signal for `EntryPointKind::Test` (0.95) before falling back to
    /// the file-path / name-prefix heuristics. `false` for file nodes,
    /// for symbols whose indexer doesn't stamp the role bit, and for
    /// symbols restored from pre-PR-F1 (v2 or earlier) artifacts.
    #[serde(default)]
    pub is_test: bool,
    /// Iteration 26: per-function complexity metrics (cyclomatic, cognitive,
    /// nloc, max_nesting, param_count) computed by
    /// [`crate::complexity::ComplexityWalker`] over the host file's tree-
    /// sitter AST. Populated only for function-like SCIP symbols
    /// (`Function` / `Method` / `Constructor`) and only when the file's
    /// language is supported by the walker AND a tree-sitter range can be
    /// matched against the SCIP definition. `None` for file nodes,
    /// non-function symbols, unsupported languages, and for any node
    /// restored from a pre-iteration-26 (v8 or earlier) artifact —
    /// `#[serde(default)]` keeps the deserialization tolerant in that
    /// case, but the version bump forces a re-warm.
    #[serde(default)]
    pub complexity: Option<ComplexityMetrics>,
}

impl RepoGraphNode {
    pub fn key(&self) -> RepoNodeKey {
        self.id.clone()
    }

    pub fn kind(&self) -> RepoGraphNodeKind {
        self.kind
    }

    fn intrinsic_weight(&self) -> f64 {
        match self.kind {
            RepoGraphNodeKind::File => 1.0,
            RepoGraphNodeKind::Symbol => match self.symbol_kind {
                Some(ScipSymbolKind::Type)
                | Some(ScipSymbolKind::Struct)
                | Some(ScipSymbolKind::Interface)
                | Some(ScipSymbolKind::Enum) => SYMBOL_KIND_TYPE_MULTIPLIER,
                Some(ScipSymbolKind::Method) | Some(ScipSymbolKind::Constructor) => {
                    SYMBOL_KIND_METHOD_MULTIPLIER
                }
                Some(ScipSymbolKind::Function) => SYMBOL_KIND_FUNCTION_MULTIPLIER,
                Some(ScipSymbolKind::Variable)
                | Some(ScipSymbolKind::Field)
                | Some(ScipSymbolKind::Property)
                | Some(ScipSymbolKind::Constant) => SYMBOL_KIND_VARIABLE_MULTIPLIER,
                _ => SYMBOL_KIND_DEFAULT_MULTIPLIER,
            },
            // PR F2: process nodes are synthetic side-channel metadata.
            // Give them the lowest tier (variable-class) so PageRank
            // doesn't promote them above real symbols just because
            // they fan out to many steps.
            RepoGraphNodeKind::Process => SYMBOL_KIND_VARIABLE_MULTIPLIER,
            // Database tables are pure sinks — they only receive
            // `Reads`/`Writes` edges from caller symbols. Same tier as
            // `Process` so PageRank doesn't promote them just because
            // many functions touch the same table.
            RepoGraphNodeKind::Table => SYMBOL_KIND_VARIABLE_MULTIPLIER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGraphEdgeKind {
    ContainsDefinition,
    DeclaredInFile,
    FileReference,
    /// Generic symbol reference. PR A3 carves out `Reads` and `Writes`
    /// from this kind based on SCIP `SymbolRole::ReadAccess` /
    /// `WriteAccess`; this catch-all variant is still emitted for
    /// occurrences that carry neither role (e.g. `Import`, type-only
    /// references). Matches the `references` `EdgeCategory` per the
    /// inter-PR contract.
    SymbolReference,
    /// PR A3: SCIP `SymbolRole::ReadAccess` reference. Occurrences
    /// where the symbol is loaded/used without being written.
    Reads,
    /// PR A3: SCIP `SymbolRole::WriteAccess` reference. Occurrences
    /// where the symbol is assigned to or otherwise mutated.
    Writes,
    /// SCIP `Relationship.is_reference` — subtype-of / supertype-of.
    /// Used by scip-typescript for `class Foo extends Bar`, by
    /// rust-analyzer for supertrait references, and as a generic
    /// upward-typing pointer for cross-symbol relationships that aren't
    /// covered by the more specific variants below. Renamed from
    /// `SymbolRelationshipReference` in artifact v6 (PR clarity rename).
    Extends,
    /// SCIP `Relationship.is_implementation` — interface / trait
    /// implementation. `impl Trait for Struct` in Rust, `class Foo
    /// implements Bar` in TypeScript / Java, `class Child(Parent)` for
    /// ABC implementations in Python. Renamed from
    /// `SymbolRelationshipImplementation` in artifact v6.
    Implements,
    /// SCIP `Relationship.is_type_definition` — variable / parameter /
    /// return type, type alias target, generic bound. The receiver
    /// symbol's *type* is the target. Renamed from
    /// `SymbolRelationshipTypeDefinition` in artifact v6.
    TypeDefines,
    /// SCIP `Relationship.is_definition` — canonical-definition
    /// relationship. Rare; emitted when a symbol's definition is part of
    /// another symbol's defining region (e.g. a property defined inside
    /// a class without its own definition site). Renamed from
    /// `SymbolRelationshipDefinition` in artifact v6.
    Defines,
    /// PR F1: synthetic edge marking that the *target* symbol is an
    /// entry point of the *source* file (e.g. `src/main.rs ─EntryPointOf→
    /// fn main`). Stamped by [`crate::entry_points::detect_entry_points`]
    /// during graph build. `dead_symbols` excludes any node with an
    /// incoming `EntryPointOf` edge so test/main/HTTP-route symbols
    /// don't get false-positive flagged as dead. The edge carries the
    /// detector's per-hit confidence (0.6 – 0.95) and a `reason` string
    /// describing the matching heuristic (e.g. `"rust-main"`,
    /// `"scip-test-role"`, `"py-dunder-main"`).
    EntryPointOf,
    /// PR F3: synthesized "node X is a member of community Y" edge.
    /// Currently surfaced only via the per-graph
    /// [`crate::communities::Community`] sidecar — the variant exists in
    /// the enum (and in [`edge_confidence_floor`] / [`edge_weight`]) so
    /// downstream tools that iterate by edge kind have a stable kind
    /// name to dispatch on, even when no `MemberOf` edges are
    /// materialized into the petgraph.
    MemberOf,
    /// PR F2: synthetic edge linking a [`RepoGraphNodeKind::Process`]
    /// node to each [`RepoGraphNode`] along the deterministic call
    /// chain it traced. The 0-indexed step ordinal lives on
    /// [`RepoGraphEdge::step`] (the entry point is `step=0`, the
    /// terminal node is `step=step_count-1`). Confidence floor is 0.95 —
    /// process membership is computed from SCIP-derived edges, so it's
    /// as deterministic as the source graph.
    StepInProcess,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoGraphEdge {
    pub kind: RepoGraphEdgeKind,
    pub weight: f64,
    pub evidence_count: usize,
    /// Edge confidence in [0, 1]. Comes from a per-kind floor (PR A2 plan)
    /// optionally adjusted by the visibility heuristic. `min_confidence`
    /// filters in `code_graph.impact` use this value.
    pub confidence: f64,
    /// Optional human-readable explanation for the confidence value
    /// (e.g. `"local-prefix"` when one of the involved symbols is
    /// document-local). `None` means "default floor for kind, no
    /// adjustments applied".
    pub reason: Option<String>,
    /// PR F2: 0-indexed step ordinal — only populated on
    /// [`RepoGraphEdgeKind::StepInProcess`] edges. `None` for every
    /// other kind. Stored as a dedicated field rather than reusing
    /// `weight` so PageRank / shortest-path scoring stays oblivious to
    /// the process side-channel.
    #[serde(default)]
    pub step: Option<i32>,
}

/// Initial confidence floor for an edge of the given kind.
///
/// See the constants block at the top of this module for the table; the
/// values are tuned to put high-trust SCIP-derived edges (definitions,
/// declarations) above 0.9 and looser cross-symbol relationship edges in
/// the 0.8 band.
pub fn edge_confidence_floor(kind: RepoGraphEdgeKind) -> f64 {
    match kind {
        RepoGraphEdgeKind::ContainsDefinition => EDGE_CONFIDENCE_CONTAINS_DEFINITION,
        RepoGraphEdgeKind::DeclaredInFile => EDGE_CONFIDENCE_DECLARED_IN_FILE,
        RepoGraphEdgeKind::FileReference => EDGE_CONFIDENCE_FILE_REFERENCE,
        RepoGraphEdgeKind::SymbolReference => EDGE_CONFIDENCE_SYMBOL_REFERENCE,
        RepoGraphEdgeKind::Reads => EDGE_CONFIDENCE_READS,
        RepoGraphEdgeKind::Writes => EDGE_CONFIDENCE_WRITES,
        RepoGraphEdgeKind::Extends => EDGE_CONFIDENCE_EXTENDS,
        RepoGraphEdgeKind::Implements => EDGE_CONFIDENCE_IMPLEMENTS,
        RepoGraphEdgeKind::TypeDefines => EDGE_CONFIDENCE_TYPE_DEFINES,
        RepoGraphEdgeKind::Defines => EDGE_CONFIDENCE_DEFINES,
        RepoGraphEdgeKind::EntryPointOf => EDGE_CONFIDENCE_ENTRY_POINT_OF,
        RepoGraphEdgeKind::MemberOf => EDGE_CONFIDENCE_MEMBER_OF,
        RepoGraphEdgeKind::StepInProcess => EDGE_CONFIDENCE_STEP_IN_PROCESS,
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

#[derive(Default)]
struct RepoDependencyGraphBuilder {
    graph: DiGraph<RepoGraphNode, RepoGraphEdge>,
    node_lookup: BTreeMap<RepoNodeKey, NodeIndex>,
    edge_accumulator: BTreeMap<(NodeIndex, NodeIndex, RepoGraphEdgeKind), usize>,
    symbol_file: BTreeMap<String, PathBuf>,
    symbol_language: BTreeMap<String, String>,
    declared_symbols: BTreeSet<String>,
    /// Accumulator for the per-file `SymbolRange` sidecar. Unsorted; the
    /// builder sorts each entry by `start_line` in `finish()`.
    symbol_ranges: BTreeMap<PathBuf, Vec<SymbolRange>>,
    /// Project clone root, when known. When set, edge classification can
    /// fall back to a tree-sitter-based access classifier for occurrences
    /// whose SCIP indexer didn't populate `ReadAccess`/`WriteAccess`
    /// roles (notably rust-analyzer). When `None`, classification stays
    /// SCIP-only — used by unit tests that pass synthetic indices with
    /// no on-disk file backing.
    project_root: Option<PathBuf>,
    classifier: crate::access_classifier::AccessClassifier,
    /// Per-file source-text cache. `None` means a previous read failed
    /// (file outside project root, missing, not UTF-8) — re-cached so
    /// we don't keep retrying.
    source_cache: BTreeMap<PathBuf, Option<String>>,
}

impl RepoDependencyGraphBuilder {
    fn add_index(&mut self, index: &ParsedScipIndex) {
        for external_symbol in &index.external_symbols {
            self.ensure_symbol_node(external_symbol, None, None, true);
        }

        for file in &index.files {
            self.add_file(file);
        }
    }

    fn add_file(&mut self, file: &ScipFile) {
        let file_index = self.ensure_file_node(&file.relative_path, &file.language);

        for symbol in &file.symbols {
            let symbol_index = self.ensure_symbol_node(
                symbol,
                Some(&file.relative_path),
                Some(&file.language),
                false,
            );
            self.symbol_file
                .insert(symbol.symbol.clone(), file.relative_path.clone());
            self.symbol_language
                .insert(symbol.symbol.clone(), file.language.clone());
            self.declared_symbols.insert(symbol.symbol.clone());
            self.bump_edge(
                symbol_index,
                file_index,
                RepoGraphEdgeKind::DeclaredInFile,
                1,
            );
            self.bump_edge(
                file_index,
                symbol_index,
                RepoGraphEdgeKind::ContainsDefinition,
                1,
            );

            for relationship in &symbol.relationships {
                self.add_relationship(symbol_index, relationship);
            }
        }

        for definition in &file.definitions {
            if let Some(symbol_index) = self.ensure_known_symbol_from_occurrence(definition, file) {
                self.bump_edge(
                    file_index,
                    symbol_index,
                    RepoGraphEdgeKind::ContainsDefinition,
                    1,
                );
                self.bump_edge(
                    symbol_index,
                    file_index,
                    RepoGraphEdgeKind::DeclaredInFile,
                    1,
                );
                self.record_symbol_range(symbol_index, file, definition);
                // PR F1: propagate the SCIP `Test` role from the
                // definition occurrence onto the symbol node so the
                // entry-point detector can use it as the high-confidence
                // signal for test detection. SCIP `Test` is bit 32 on
                // `SymbolRole`; not every indexer stamps it (the Rust
                // scip-rust shipped as of 2026-04 does not), which is
                // why we keep the file-path / name-prefix heuristics in
                // [`crate::entry_points`] as a fallback.
                if definition.roles.contains(&ScipSymbolRole::Test) {
                    self.graph[symbol_index].is_test = true;
                }
            }
        }

        for reference in &file.references {
            self.add_reference(file_index, file, reference);
        }
    }

    fn add_reference(
        &mut self,
        source_file_index: NodeIndex,
        file: &ScipFile,
        occurrence: &ScipOccurrence,
    ) {
        let symbol_index = self.ensure_symbol_node_from_occurrence(occurrence, file);
        self.bump_edge(
            source_file_index,
            symbol_index,
            RepoGraphEdgeKind::FileReference,
            1,
        );

        if let Some(target_file) = self.symbol_file.get(&occurrence.symbol).cloned() {
            let target_file_index = self.ensure_file_node(&target_file, file.language.as_str());
            self.bump_edge(
                source_file_index,
                target_file_index,
                RepoGraphEdgeKind::FileReference,
                1,
            );
            // PR A3: split symbol-to-file references on SCIP role flags so
            // `code_graph neighbors --kind_filter=writes` can pick out
            // mutators of a field. SCIP can stamp both `ReadAccess` and
            // `WriteAccess` on the same occurrence (e.g. `x += 1`); when
            // both flags are present we treat it as a write since the
            // mutation is the more load-bearing signal for callers asking
            // "who changes X".
            //
            // Indexer-quality fallback: when neither role bit is set
            // (notably rust-analyzer, which emits no access roles at all),
            // consult the tree-sitter `AccessClassifier` to recover the
            // read/write distinction from AST context. Only fires when
            // the builder was created via `build_with_source` with a
            // project root — `build` keeps the SCIP-only fast path for
            // unit tests with synthetic indices.
            let edge_kind = self.classify_reference_edge_kind(file, occurrence);
            self.bump_edge(symbol_index, target_file_index, edge_kind, 1);
        }
    }

    /// Classify the symbol→target_file reference edge for an occurrence.
    /// SCIP role bits are the primary signal. When the indexer didn't
    /// populate either `ReadAccess` or `WriteAccess` (rust-analyzer is
    /// the canonical case), fall back to the tree-sitter
    /// [`crate::access_classifier::AccessClassifier`] which derives the
    /// distinction from AST context (`assignment_expression` LHS, etc.).
    /// The fallback only fires when the builder has a `project_root`
    /// and the occurrence's file is readable as UTF-8.
    fn classify_reference_edge_kind(
        &mut self,
        file: &ScipFile,
        occurrence: &ScipOccurrence,
    ) -> RepoGraphEdgeKind {
        if occurrence.roles.contains(&ScipSymbolRole::WriteAccess) {
            return RepoGraphEdgeKind::Writes;
        }
        if occurrence.roles.contains(&ScipSymbolRole::ReadAccess) {
            return RepoGraphEdgeKind::Reads;
        }
        let Some(root) = self.project_root.as_ref() else {
            return RepoGraphEdgeKind::SymbolReference;
        };
        // Read-and-cache the file source. Failures are negative-cached
        // so subsequent occurrences in the same file don't re-stat.
        let rel = file.relative_path.clone();
        if !self.source_cache.contains_key(&rel) {
            let abs = root.join(&rel);
            let read = std::fs::read_to_string(&abs).ok();
            self.source_cache.insert(rel.clone(), read);
        }
        let Some(source) = self.source_cache.get(&rel).and_then(|s| s.as_deref()) else {
            return RepoGraphEdgeKind::SymbolReference;
        };
        let kind = self.classifier.classify(
            file.language.as_str(),
            source,
            occurrence.range.start_line as u32,
            occurrence.range.start_character as u32,
        );
        use crate::access_classifier::AccessKind;
        match kind {
            AccessKind::Write | AccessKind::ReadWrite => RepoGraphEdgeKind::Writes,
            AccessKind::Read => RepoGraphEdgeKind::Reads,
            AccessKind::NotAnAccess | AccessKind::Unknown => RepoGraphEdgeKind::SymbolReference,
        }
    }

    fn add_relationship(
        &mut self,
        source_symbol_index: NodeIndex,
        relationship: &ScipRelationship,
    ) {
        let target_symbol_index = self.ensure_placeholder_symbol_node(&relationship.target_symbol);

        for kind in &relationship.kinds {
            let edge_kind = match kind {
                ScipRelationshipKind::Reference => RepoGraphEdgeKind::Extends,
                ScipRelationshipKind::Implementation => RepoGraphEdgeKind::Implements,
                ScipRelationshipKind::TypeDefinition => RepoGraphEdgeKind::TypeDefines,
                ScipRelationshipKind::Definition => RepoGraphEdgeKind::Defines,
            };
            self.bump_edge(source_symbol_index, target_symbol_index, edge_kind, 1);
        }
    }

    fn ensure_file_node(&mut self, path: &Path, language: &str) -> NodeIndex {
        let key = RepoNodeKey::File(path.to_path_buf());
        if let Some(index) = self.node_lookup.get(&key) {
            return *index;
        }

        let display_name = path.display().to_string();
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::File,
            display_name,
            language: Some(language.to_string()),
            file_path: Some(path.to_path_buf()),
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
        };
        let node_index = self.graph.add_node(node);
        self.node_lookup.insert(key, node_index);
        node_index
    }

    fn ensure_symbol_node(
        &mut self,
        symbol: &ScipSymbol,
        file_path: Option<&Path>,
        language: Option<&str>,
        is_external: bool,
    ) -> NodeIndex {
        let key = RepoNodeKey::Symbol(symbol.symbol.clone());
        if let Some(index) = self.node_lookup.get(&key) {
            return *index;
        }

        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::Symbol,
            display_name: symbol
                .display_name
                .clone()
                .unwrap_or_else(|| symbol.symbol.clone()),
            language: language.map(ToOwned::to_owned),
            file_path: file_path.map(Path::to_path_buf),
            symbol: Some(symbol.symbol.clone()),
            symbol_kind: symbol.kind.clone(),
            is_external,
            visibility: symbol.visibility,
            signature: symbol.signature.clone(),
            documentation: symbol.documentation.clone(),
            signature_parts: symbol.signature_parts.clone(),
            is_test: false,
            complexity: None,
        };
        let node_index = self.graph.add_node(node);
        self.node_lookup.insert(key, node_index);
        node_index
    }

    fn ensure_known_symbol_from_occurrence(
        &mut self,
        occurrence: &ScipOccurrence,
        file: &ScipFile,
    ) -> Option<NodeIndex> {
        self.declared_symbols
            .contains(&occurrence.symbol)
            .then(|| self.ensure_symbol_node_from_occurrence(occurrence, file))
    }

    fn ensure_symbol_node_from_occurrence(
        &mut self,
        occurrence: &ScipOccurrence,
        file: &ScipFile,
    ) -> NodeIndex {
        if let Some(index) = self
            .node_lookup
            .get(&RepoNodeKey::Symbol(occurrence.symbol.clone()))
            .copied()
        {
            return index;
        }

        let symbol = ScipSymbol {
            symbol: occurrence.symbol.clone(),
            kind: None,
            display_name: Some(occurrence.symbol.clone()),
            signature: None,
            documentation: Vec::new(),
            relationships: Vec::new(),
            visibility: Some(crate::scip_parser::ScipVisibility::from_symbol_identifier(
                &occurrence.symbol,
            )),
            signature_parts: None,
        };
        self.ensure_symbol_node(
            &symbol,
            Some(&file.relative_path),
            Some(&file.language),
            false,
        )
    }

    fn ensure_placeholder_symbol_node(&mut self, symbol: &str) -> NodeIndex {
        if let Some(index) = self
            .node_lookup
            .get(&RepoNodeKey::Symbol(symbol.to_string()))
            .copied()
        {
            return index;
        }

        let file_path = self.symbol_file.get(symbol).cloned();
        let language = self.symbol_language.get(symbol).cloned();
        let is_external = !self.declared_symbols.contains(symbol);
        let node = RepoGraphNode {
            id: RepoNodeKey::Symbol(symbol.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: symbol.to_string(),
            language,
            file_path,
            symbol: Some(symbol.to_string()),
            symbol_kind: None,
            is_external,
            visibility: Some(ScipVisibility::from_symbol_identifier(symbol)),
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
        };
        let key = node.id.clone();
        let node_index = self.graph.add_node(node);
        self.node_lookup.insert(key, node_index);
        node_index
    }

    fn bump_edge(
        &mut self,
        source: NodeIndex,
        target: NodeIndex,
        kind: RepoGraphEdgeKind,
        count: usize,
    ) {
        *self
            .edge_accumulator
            .entry((source, target, kind))
            .or_default() += count;
    }

    /// Record the definition's enclosing range (if any) into the sidecar
    /// `symbol_ranges` map. SCIP lines are 0-indexed on the wire; we
    /// normalize to the 1-indexed inclusive convention used by callers.
    fn record_symbol_range(
        &mut self,
        symbol_index: NodeIndex,
        file: &ScipFile,
        occurrence: &ScipOccurrence,
    ) {
        let Some(enclosing) = occurrence.enclosing_range.as_ref() else {
            return;
        };
        let start_line = (enclosing.start_line.max(0) as u32).saturating_add(1);
        let end_line = (enclosing.end_line.max(0) as u32).saturating_add(1);
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };
        self.symbol_ranges
            .entry(file.relative_path.clone())
            .or_default()
            .push(SymbolRange {
                start_line,
                end_line,
                node: symbol_index,
            });
    }

    fn finish(mut self) -> RepoDependencyGraph {
        for ((source, target, kind), evidence_count) in self.edge_accumulator {
            let (confidence, reason) =
                derive_edge_confidence(&self.graph, source, target, kind);
            self.graph.add_edge(
                source,
                target,
                RepoGraphEdge {
                    kind,
                    weight: edge_weight(kind) * (evidence_count as f64),
                    evidence_count,
                    confidence,
                    reason,
                    step: None,
                },
            );
        }

        // Sort each per-file range vec by `start_line` so callers can reason
        // about ordering even though nesting still demands a linear overlap
        // scan.
        for ranges in self.symbol_ranges.values_mut() {
            ranges.sort_by_key(|r| (r.start_line, r.end_line));
        }

        let name_index = build_name_index(&self.graph);
        let mut graph = RepoDependencyGraph {
            graph: self.graph,
            node_lookup: self.node_lookup,
            name_index,
            symbol_ranges: self.symbol_ranges,
            communities: Vec::new(),
            community_lookup: BTreeMap::new(),
            processes: Vec::new(),
            process_lookup: BTreeMap::new(),
        };

        // PR F3: run modularity-based community detection unless the
        // feature flag is explicitly turned off. The detector is
        // O((V + E) × iterations); on a 12k-node, 150k-edge canonical
        // graph it lands in the ~hundreds-of-ms range — comparable to
        // the SCC pass that already runs in `derive_graph_caches`.
        if crate::communities::detection_enabled() {
            let communities = crate::communities::detect_communities(&graph);
            graph.install_communities(communities);
        }

        graph
    }
}

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
fn build_process_lookup(
    processes: &[crate::processes::Process],
) -> BTreeMap<usize, Vec<usize>> {
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
fn attach_complexity_metrics<F>(graph: &mut RepoDependencyGraph, mut load_source: F)
where
    F: FnMut(&Path) -> Option<String>,
{
    // Collect candidate files first: any file with at least one function-
    // like symbol node and a non-empty `language`. The symbol_ranges
    // sidecar already keys on PathBuf and gives us 1-indexed inclusive
    // ranges per node, so we use it as the iteration root.
    let candidates: Vec<(PathBuf, String, Vec<(NodeIndex, u32, u32, Option<String>)>)> = graph
        .symbol_ranges_by_file()
        .filter_map(|(path, ranges)| {
            // Take the first function-like node we find in this file just
            // to read the language hint (every node in a file shares the
            // SCIP `Document.language`, so any one works). Skip files
            // without a function-like node — nothing to compute.
            let mut entries: Vec<(NodeIndex, u32, u32, Option<String>)> = Vec::new();
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
                if name_hit.is_none() {
                    if let (Some(disp), Some(fn_name)) = (display_name.as_deref(), fm.name.as_deref())
                    {
                        if names_match(disp, fn_name) {
                            name_hit = Some(i);
                        }
                    }
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
    if let Some((_, tail)) = scip_display.rsplit_once("::") {
        if tail == ts_name {
            return true;
        }
    }
    if let Some((_, tail)) = scip_display.rsplit_once('.') {
        if tail == ts_name {
            return true;
        }
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
        let mut processes_out: Vec<RepoGraphArtifactProcess> = Vec::with_capacity(
            self.processes.len(),
        );
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

        let mut surviving_symbol_ranges: BTreeMap<
            PathBuf,
            Vec<RepoGraphArtifactSymbolRange>,
        > = BTreeMap::new();
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

/// PR F1: public wrapper around the per-kind weight table so the
/// entry-point detector (which lives in a sibling module and assembles
/// `EntryPointOf` edges by hand) can stay in sync with the build-time
/// weight assignments.
pub(crate) fn edge_weight_for(kind: RepoGraphEdgeKind) -> f64 {
    edge_weight(kind)
}

fn edge_weight(kind: RepoGraphEdgeKind) -> f64 {
    match kind {
        RepoGraphEdgeKind::ContainsDefinition => EDGE_WEIGHT_DEFINITION_TO_FILE,
        RepoGraphEdgeKind::DeclaredInFile => EDGE_WEIGHT_FILE_TO_DEFINITION,
        RepoGraphEdgeKind::FileReference => EDGE_WEIGHT_FILE_REFERENCE,
        // PR A3: `Reads` and `Writes` are refinements of `SymbolReference`;
        // they reuse the same structural weight so PageRank / shortest-path
        // results are stable across the split.
        RepoGraphEdgeKind::SymbolReference
        | RepoGraphEdgeKind::Reads
        | RepoGraphEdgeKind::Writes => EDGE_WEIGHT_SYMBOL_REFERENCE,
        RepoGraphEdgeKind::Extends => EDGE_WEIGHT_EXTENDS,
        RepoGraphEdgeKind::Implements => EDGE_WEIGHT_IMPLEMENTS,
        RepoGraphEdgeKind::TypeDefines => EDGE_WEIGHT_TYPE_DEFINES,
        RepoGraphEdgeKind::Defines => EDGE_WEIGHT_DEFINES,
        RepoGraphEdgeKind::EntryPointOf => EDGE_WEIGHT_ENTRY_POINT_OF,
        RepoGraphEdgeKind::MemberOf => EDGE_WEIGHT_MEMBER_OF,
        RepoGraphEdgeKind::StepInProcess => EDGE_WEIGHT_STEP_IN_PROCESS,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use petgraph::visit::EdgeRef;

    use super::*;
    use crate::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
        ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
    };

    #[test]
    fn builds_dependency_graph_with_file_and_symbol_metadata() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);

        // PR F2 bumped the count from 5 to 6: the entry-point detector
        // tags `main` as `EntryPointKind::Main`, and the process
        // detector traces a (short) call chain from it, materializing
        // one synthetic `Process` node. The five SCIP-derived nodes
        // (2 files + 3 symbols) are still all present; the extra node
        // is the synthetic process.
        assert_eq!(graph.node_count(), 6);
        assert!(graph.edge_count() >= 8);

        let app_file = graph
            .file_node("src/app.rs")
            .expect("app file node should exist");
        let app_node = graph.node(app_file);
        assert_eq!(app_node.kind, RepoGraphNodeKind::File);
        assert_eq!(app_node.language.as_deref(), Some("rust"));
        assert_eq!(app_node.file_path.as_deref(), Some(Path::new("src/app.rs")));

        let helper_symbol = graph
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .expect("helper symbol node should exist");
        let helper_node = graph.node(helper_symbol);
        assert_eq!(helper_node.kind, RepoGraphNodeKind::Symbol);
        assert_eq!(helper_node.symbol_kind, Some(ScipSymbolKind::Function));
        assert_eq!(
            helper_node.file_path.as_deref(),
            Some(Path::new("src/helper.rs"))
        );

        let has_file_reference = graph.graph().edges(app_file).any(|edge| {
            edge.target() == helper_symbol && edge.weight().kind == RepoGraphEdgeKind::FileReference
        });
        assert!(has_file_reference, "expected file->symbol reference edge");
    }

    /// Regression test for the sparse PageRank replacement.  Validates
    /// the three properties the downstream ranking code depends on:
    ///
    /// 1. Output length matches node count (required by indexing in
    ///    `rank()`).
    /// 2. All ranks are finite, non-negative, and sum to ~1 (mass
    ///    preservation + normalization — a sanity check against FP
    ///    drift or dangling-node mass loss).
    /// 3. Isolated nodes (no in-edges, no out-edges) share the same
    ///    rank — they receive only the random-jump + dangling baseline.
    ///
    /// Does NOT assert numerical equivalence with petgraph's 0.8.3
    /// `page_rank` — that implementation uses a different per-pair
    /// formulation, so direct comparison is meaningful only in the
    /// ordering test above.
    #[test]
    fn compute_pagerank_sparse_is_mass_preserving_and_finite() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranks =
            compute_pagerank_sparse(&graph.graph, PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS);

        assert_eq!(ranks.len(), graph.node_count());
        assert!(ranks.iter().all(|r| r.is_finite() && *r >= 0.0));
        let sum: f64 = ranks.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "ranks must sum to ~1, got {sum}");
    }

    #[test]
    fn compute_pagerank_sparse_handles_empty_graph() {
        let graph: DiGraph<RepoGraphNode, RepoGraphEdge> = DiGraph::new();
        let ranks = compute_pagerank_sparse(&graph, PAGE_RANK_DAMPING_FACTOR, 5);
        assert!(ranks.is_empty());
    }

    #[test]
    fn page_rank_ordering_favors_referenced_symbols_and_files() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();

        let helper_symbol_rank = ranking
            .nodes
            .iter()
            .position(|node| {
                node.key
                    == RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string())
            })
            .expect("helper symbol should be ranked");
        let app_symbol_rank = ranking
            .nodes
            .iter()
            .position(|node| {
                node.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string())
            })
            .expect("main symbol should be ranked");
        let helper_file_rank = ranking
            .nodes
            .iter()
            .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/helper.rs")))
            .expect("helper file should be ranked");
        let app_file_rank = ranking
            .nodes
            .iter()
            .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/app.rs")))
            .expect("app file should be ranked");

        // PR F4: positions are now governed by fused rank (RRF over
        // pagerank, total degree, entry-point distance), so we no
        // longer assert the legacy "helper outranks main" position
        // ordering — `main` is an entry point and the fusion now
        // promotes it. The classic `pagerank * structural_weight`
        // score is still surfaced on the node for callers that want
        // it, and we keep asserting that signal directly so the
        // PageRank pass itself doesn't silently regress.
        let helper_symbol_score = ranking.nodes[helper_symbol_rank].score;
        let app_symbol_score = ranking.nodes[app_symbol_rank].score;
        assert!(helper_symbol_score > app_symbol_score);

        let helper_file_score = ranking.nodes[helper_file_rank].score;
        let app_file_score = ranking.nodes[app_file_rank].score;
        assert!(helper_file_score > app_file_score);
    }

    /// PR F4: the entry-point function detected for the fixture
    /// (`fn main` in `src/app.rs`) must come back from `rank()` with
    /// `entry_point_distance == Some(0)` — distance is measured from
    /// the entry-point set itself, BFS via Outgoing edges.
    #[test]
    fn entry_point_distance_zero_at_entry_point() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();
        let main_node = ranking
            .nodes
            .iter()
            .find(|n| {
                n.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string())
            })
            .expect("main symbol should be in ranking");
        assert!(
            main_node.is_entry_point,
            "fixture's `fn main` should have been detected as an entry point",
        );
        assert_eq!(
            main_node.entry_point_distance,
            Some(0),
            "entry-point function should sit at distance 0",
        );
    }

    /// PR F4: build a tiny synthetic graph with two symbols at
    /// identical PageRank — one is the entry point, the other is a
    /// helper that lives off to the side. With RRF the entry-point
    /// signal breaks the tie and the entry point ranks higher.
    #[test]
    fn rrf_fused_rank_promotes_entry_points_under_pagerank_tie() {
        // Hand-build the ranked node vector to control the inputs
        // exactly — using the SCIP fixture pulls in too much
        // structural variation to guarantee a strict pagerank tie.
        let entry_key = RepoNodeKey::Symbol("symbol:entry".to_string());
        let helper_key = RepoNodeKey::Symbol("symbol:helper".to_string());
        let mut nodes = vec![
            RankedRepoGraphNode {
                node_index: NodeIndex::new(0),
                key: entry_key.clone(),
                kind: RepoGraphNodeKind::Symbol,
                score: 0.5,
                page_rank: 0.5,
                structural_weight: 1.0,
                inbound_edge_weight: 1.0,
                outbound_edge_weight: 1.0,
                is_entry_point: true,
                entry_point_distance: Some(0),
                fused_rank: 0.0,
            },
            RankedRepoGraphNode {
                node_index: NodeIndex::new(1),
                key: helper_key.clone(),
                kind: RepoGraphNodeKind::Symbol,
                score: 0.5,
                page_rank: 0.5,
                structural_weight: 1.0,
                inbound_edge_weight: 1.0,
                outbound_edge_weight: 1.0,
                is_entry_point: false,
                entry_point_distance: None,
                fused_rank: 0.0,
            },
        ];
        apply_rrf_fused_rank(&mut nodes);
        nodes.sort_by(|l, r| r.fused_rank.total_cmp(&l.fused_rank));
        assert_eq!(
            nodes[0].key, entry_key,
            "entry point must outrank helper under RRF when pagerank/degree are tied",
        );
        assert_eq!(nodes[1].key, helper_key);
        assert!(
            nodes[0].fused_rank > nodes[1].fused_rank,
            "fused rank for entry point ({}) should exceed helper ({})",
            nodes[0].fused_rank,
            nodes[1].fused_rank,
        );
    }

    fn fixture_index() -> ParsedScipIndex {
        let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
        let helper_symbol = ScipSymbol {
            symbol: helper_symbol_name.clone(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("helper".to_string()),
            signature: Some("fn helper()".to_string()),
            documentation: vec!["returns a value".to_string()],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let trait_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some("HelperTrait".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let main_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("main".to_string()),
            signature: Some("fn main()".to_string()),
            documentation: vec![],
            relationships: vec![ScipRelationship {
                source_symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
                target_symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
                kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
            }],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };

        ParsedScipIndex {
            metadata: ScipMetadata {
                project_root: Some("file:///workspace/repo".to_string()),
                tool_name: Some("rust-analyzer".to_string()),
                tool_version: Some("1.0.0".to_string()),
            },
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/helper.rs"),
                    definitions: vec![definition_occurrence(&helper_symbol_name)],
                    references: vec![],
                    occurrences: vec![definition_occurrence(&helper_symbol_name)],
                    symbols: vec![helper_symbol],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/app.rs"),
                    definitions: vec![definition_occurrence(&main_symbol.symbol)],
                    references: vec![reference_occurrence(&helper_symbol_name)],
                    occurrences: vec![
                        definition_occurrence(&main_symbol.symbol),
                        reference_occurrence(&helper_symbol_name),
                    ],
                    symbols: vec![main_symbol, trait_symbol],
                },
            ],
            external_symbols: vec![],
        }
    }

    fn definition_occurrence(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 6,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    fn reference_occurrence(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    /// v8 end-to-end: simulate a rust-analyzer SCIP feed (no `ReadAccess`
    /// or `WriteAccess` role bits on any occurrence) against a real
    /// on-disk Rust file. The classifier should recover the read/write
    /// distinction from AST context, producing `Reads` and `Writes`
    /// edges where v7 would have emitted only `SymbolReference`.
    ///
    /// This is the reference verification for the `build_with_source`
    /// path — proves the fallback fires end-to-end and not just in the
    /// classifier's unit tests.
    #[test]
    fn build_with_source_recovers_reads_and_writes_when_scip_roles_absent() {
        // A real Rust source file where `counter` is written on one line
        // and read on another. `value` is a definition site (not an
        // access). The line/column positions below match these
        // identifiers exactly.
        //
        // Layout (0-indexed):
        //   line 0: pub static mut COUNTER: i32 = 0;
        //   line 1:
        //   line 2: pub fn bump() {
        //   line 3:     unsafe { COUNTER = COUNTER + 1; }
        //                        ^^^^^^^   ^^^^^^^
        //                        col 13    col 23
        //   line 4: }
        let source = "pub static mut COUNTER: i32 = 0;\n\n\
                      pub fn bump() {\n    \
                      unsafe { COUNTER = COUNTER + 1; }\n}\n";
        let tempdir = tempfile::tempdir().expect("tempdir");
        let rel = PathBuf::from("src/counter.rs");
        let abs = tempdir.path().join(&rel);
        std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
        std::fs::write(&abs, source).expect("write");

        let counter_symbol = "scip-rust pkg src/counter.rs `COUNTER`.".to_string();
        let counter_def_sym = ScipSymbol {
            symbol: counter_symbol.clone(),
            kind: Some(ScipSymbolKind::Variable),
            display_name: Some("COUNTER".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
            signature_parts: None,
        };

        // Definition occurrence at line 0 (the `static mut COUNTER`).
        let def = ScipOccurrence {
            symbol: counter_symbol.clone(),
            range: ScipRange {
                start_line: 0,
                start_character: 15,
                end_line: 0,
                end_character: 22,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        };
        // Reference occurrence at line 3 column 13 (`COUNTER = …` LHS) —
        // EMPTY role bits, mirroring rust-analyzer's SCIP output.
        let write_ref = ScipOccurrence {
            symbol: counter_symbol.clone(),
            range: ScipRange {
                start_line: 3,
                start_character: 13,
                end_line: 3,
                end_character: 20,
            },
            enclosing_range: None,
            roles: BTreeSet::new(),
            syntax_kind: None,
            override_documentation: vec![],
        };
        // Reference occurrence at line 3 column 23 (`= COUNTER + 1` RHS).
        let read_ref = ScipOccurrence {
            symbol: counter_symbol.clone(),
            range: ScipRange {
                start_line: 3,
                start_character: 23,
                end_line: 3,
                end_character: 30,
            },
            enclosing_range: None,
            roles: BTreeSet::new(),
            syntax_kind: None,
            override_documentation: vec![],
        };

        let index = ParsedScipIndex {
            metadata: ScipMetadata {
                project_root: Some("file:///workspace/repo".to_string()),
                tool_name: Some("rust-analyzer".to_string()),
                tool_version: Some("1.0.0".to_string()),
            },
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: rel.clone(),
                definitions: vec![def.clone()],
                references: vec![write_ref.clone(), read_ref.clone()],
                occurrences: vec![def, write_ref, read_ref],
                symbols: vec![counter_def_sym],
            }],
            external_symbols: vec![],
        };

        let graph = RepoDependencyGraph::build_with_source(&[index], Some(tempdir.path()));

        // Walk every edge with the COUNTER symbol as source and the
        // counter.rs file as target — there should be exactly one
        // `Writes` and one `Reads`, no `SymbolReference` fallbacks.
        let symbol_idx = graph
            .symbol_node(&counter_symbol)
            .expect("COUNTER symbol node should exist");
        let file_idx = graph
            .file_node(&rel)
            .expect("counter.rs file node should exist");
        let mut writes = 0usize;
        let mut reads = 0usize;
        let mut other = 0usize;
        for edge in graph
            .graph()
            .edges_directed(symbol_idx, petgraph::Direction::Outgoing)
        {
            if edge.target() != file_idx {
                continue;
            }
            match edge.weight().kind {
                RepoGraphEdgeKind::Writes => writes += 1,
                RepoGraphEdgeKind::Reads => reads += 1,
                RepoGraphEdgeKind::SymbolReference => other += 1,
                _ => {}
            }
        }
        assert_eq!(
            writes, 1,
            "tree-sitter classifier should have recovered exactly one Writes edge \
             (counter.rs `COUNTER = …` LHS)"
        );
        assert_eq!(
            reads, 1,
            "tree-sitter classifier should have recovered exactly one Reads edge \
             (counter.rs `… = COUNTER + 1` RHS)"
        );
        assert_eq!(
            other, 0,
            "no SymbolReference fallback edges should remain when the classifier \
             can resolve the AST context"
        );
    }

    #[test]
    fn artifact_round_trip_preserves_graph_structure() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let original_node_count = graph.node_count();
        let original_edge_count = graph.edge_count();

        let artifact = graph.to_artifact();
        assert_eq!(artifact.nodes.len(), original_node_count);
        assert_eq!(artifact.edges.len(), original_edge_count);

        let restored = RepoDependencyGraph::from_artifact(&artifact);
        assert_eq!(restored.node_count(), original_node_count);
        assert_eq!(restored.edge_count(), original_edge_count);

        // Verify file and symbol lookups still work after round-trip.
        assert!(restored.file_node("src/app.rs").is_some());
        assert!(restored.file_node("src/helper.rs").is_some());
        assert!(
            restored
                .symbol_node("scip-rust pkg src/helper.rs `helper`().")
                .is_some()
        );
        assert!(
            restored
                .symbol_node("scip-rust pkg src/app.rs `main`().")
                .is_some()
        );

        // Verify ranking still produces valid results.
        let ranking = restored.rank();
        assert!(!ranking.nodes.is_empty());
    }

    #[test]
    fn artifact_json_round_trip_preserves_graph() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let json = graph.serialize_artifact().expect("serialize");
        let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize");

        assert_eq!(restored.node_count(), graph.node_count());
        assert_eq!(restored.edge_count(), graph.edge_count());

        // Verify node metadata survived serialization.
        let helper_idx = restored
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .expect("helper symbol");
        let helper_node = restored.node(helper_idx);
        assert_eq!(helper_node.symbol_kind, Some(ScipSymbolKind::Function));
        assert_eq!(helper_node.display_name, "helper");
        assert_eq!(
            helper_node.file_path.as_deref(),
            Some(Path::new("src/helper.rs"))
        );

        // Verify edge metadata survived.
        let app_idx = restored.file_node("src/app.rs").expect("app file");
        let has_contains_def = restored
            .graph()
            .edges(app_idx)
            .any(|e| e.weight().kind == RepoGraphEdgeKind::ContainsDefinition);
        assert!(
            has_contains_def,
            "expected ContainsDefinition edge from app file"
        );
    }

    #[test]
    fn empty_artifact_round_trip() {
        let empty = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: vec![],
            edges: vec![],
            symbol_ranges: BTreeMap::new(),
            communities: Vec::new(),
            processes: vec![],
        };
        let json = serde_json::to_string(&empty).expect("serialize empty");
        let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize empty");
        assert_eq!(restored.node_count(), 0);
        assert_eq!(restored.edge_count(), 0);
    }

    // ── PR A2: edge confidence + reason ───────────────────────────────────

    /// Every edge kind emitted by the builder gets a confidence value within
    /// `(0, 1]`. Sweeping the fixture is the cheapest way to assert "no kind
    /// silently slipped through with the default 0.0".
    #[test]
    fn every_edge_kind_carries_a_confidence_value_pr_a2() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        assert!(graph.edge_count() > 0, "fixture must produce edges");

        let mut seen_kinds: BTreeSet<RepoGraphEdgeKind> = BTreeSet::new();
        for edge_ref in graph.graph().edge_references() {
            let edge = edge_ref.weight();
            seen_kinds.insert(edge.kind);
            assert!(
                edge.confidence > 0.0 && edge.confidence <= 1.0,
                "edge {:?} has out-of-range confidence {}",
                edge.kind,
                edge.confidence
            );
            // Confidence must equal the floor for the kind, optionally
            // dropped by exactly the local-prefix penalty when the reason
            // says so. PR F1: `EntryPointOf` edges set their own
            // confidence (per-detector, 0.6 – 0.95) so the floor check
            // doesn't apply — we just bound them in (0, 1]. PR F2:
            // `StepInProcess` edges always carry `reason="process-step"`
            // and stick to the floor; we whitelist that reason.
            if edge.kind == RepoGraphEdgeKind::EntryPointOf {
                continue;
            }
            let floor = edge_confidence_floor(edge.kind);
            match edge.reason.as_deref() {
                None => assert_eq!(edge.confidence, floor),
                Some("local-prefix") => {
                    let expected = (floor - EDGE_CONFIDENCE_LOCAL_PENALTY).clamp(0.0, 1.0);
                    assert!(
                        (edge.confidence - expected).abs() < 1e-9,
                        "local-prefix edge confidence {} != expected {} for kind {:?}",
                        edge.confidence,
                        expected,
                        edge.kind
                    );
                }
                Some("process-step") => {
                    assert_eq!(edge.kind, RepoGraphEdgeKind::StepInProcess);
                    assert!((edge.confidence - floor).abs() < 1e-9);
                }
                Some(other) => panic!("unexpected reason {other:?} on edge {:?}", edge.kind),
            }
        }
        // The fixture exercises Contains/DeclaredIn/FileRef/Reads (post-A3
        // the helper-call read-access reference is classified as `Reads`
        // rather than the generic `SymbolReference`) and an Implementation
        // relationship — every code path that can mint an edge today.
        assert!(seen_kinds.contains(&RepoGraphEdgeKind::ContainsDefinition));
        assert!(seen_kinds.contains(&RepoGraphEdgeKind::DeclaredInFile));
        assert!(seen_kinds.contains(&RepoGraphEdgeKind::FileReference));
        assert!(seen_kinds.contains(&RepoGraphEdgeKind::Reads));
        assert!(seen_kinds.contains(&RepoGraphEdgeKind::Implements));
    }

    /// Bincode round-trip preserves `confidence` and `reason` on every edge.
    /// This is the core "artifact v1" guarantee — old blobs without these
    /// fields will fail to deserialize and trigger a warm rebuild.
    #[test]
    fn bincode_round_trip_preserves_edge_confidence_and_reason_pr_a2() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let artifact = graph.to_artifact();
        assert_eq!(artifact.version, REPO_GRAPH_ARTIFACT_VERSION);

        // Snapshot original (kind, confidence, reason) tuples by sorting on
        // (kind, confidence, reason) — edge_count is small so a Vec<_> is
        // fine.
        let mut original: Vec<(RepoGraphEdgeKind, f64, Option<String>)> = artifact
            .edges
            .iter()
            .map(|e| (e.kind, e.confidence, e.reason.clone()))
            .collect();
        original.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.2.cmp(&b.2))
        });

        let encoded = bincode::serialize(&artifact).expect("bincode serialize");
        let decoded: RepoGraphArtifact =
            bincode::deserialize(&encoded).expect("bincode deserialize");
        assert_eq!(decoded.version, REPO_GRAPH_ARTIFACT_VERSION);

        let mut round_tripped: Vec<(RepoGraphEdgeKind, f64, Option<String>)> = decoded
            .edges
            .iter()
            .map(|e| (e.kind, e.confidence, e.reason.clone()))
            .collect();
        round_tripped.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.2.cmp(&b.2))
        });

        assert_eq!(round_tripped, original);

        // And that survives the `from_artifact` rebuild path that powers
        // `load_canonical_graph`.
        let restored = RepoDependencyGraph::from_artifact(&decoded);
        for edge_ref in restored.graph().edge_references() {
            let edge = edge_ref.weight();
            assert!(edge.confidence > 0.0 && edge.confidence <= 1.0);
        }
    }

    /// A `local`-prefixed symbol triggers `reason="local-prefix"` and a
    /// confidence drop of exactly `EDGE_CONFIDENCE_LOCAL_PENALTY` from the
    /// kind's floor. This is the only signal the visibility heuristic
    /// surfaces today.
    #[test]
    fn local_prefix_symbol_triggers_local_prefix_reason_pr_a2() {
        // Build a tiny synthetic index where one of the symbols is local.
        let local_symbol_name = "local 42".to_string();
        let local_sym = ScipSymbol {
            symbol: local_symbol_name.clone(),
            kind: Some(ScipSymbolKind::Variable),
            display_name: Some("local_var".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Private),
        signature_parts: None,
        };
        let pub_sym = ScipSymbol {
            symbol: "scip-rust pkg src/main.rs `caller`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("caller".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![ScipRelationship {
                source_symbol: "scip-rust pkg src/main.rs `caller`().".to_string(),
                target_symbol: local_symbol_name.clone(),
                kinds: BTreeSet::from([ScipRelationshipKind::Reference]),
            }],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/main.rs"),
                definitions: vec![definition_occurrence(&pub_sym.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&pub_sym.symbol)],
                symbols: vec![pub_sym, local_sym],
            }],
            external_symbols: vec![],
        };

        let graph = RepoDependencyGraph::build(&[index]);
        let mut saw_local_prefix = false;
        for edge_ref in graph.graph().edge_references() {
            let edge = edge_ref.weight();
            if edge.reason.as_deref() == Some("local-prefix") {
                saw_local_prefix = true;
                let floor = edge_confidence_floor(edge.kind);
                let expected = (floor - EDGE_CONFIDENCE_LOCAL_PENALTY).clamp(0.0, 1.0);
                assert!(
                    (edge.confidence - expected).abs() < 1e-9,
                    "expected confidence {expected}, got {} for kind {:?}",
                    edge.confidence,
                    edge.kind
                );
            }
        }
        assert!(
            saw_local_prefix,
            "expected at least one edge involving the `local 42` symbol to be flagged"
        );
    }

    // ── PR A3: SymbolReference read/write split ───────────────────────────

    /// Build a fixture project with a struct field that is read in one file
    /// and written in another. Assert the role-aware split classifies the
    /// edges as `Reads` / `Writes`. This is the core behaviour callers rely
    /// on for `code_graph neighbors --kind_filter=writes` (PR A3 acceptance).
    #[test]
    fn read_write_split_classifies_field_accesses_pr_a3() {
        // Field `Counter#value`. Lives in src/counter.rs; is written from
        // src/writer.rs (mutator) and read from src/reader.rs (observer).
        let field_symbol = "scip-rust pkg src/counter.rs `Counter`#`value`.".to_string();
        let counter_struct = ScipSymbol {
            symbol: "scip-rust pkg src/counter.rs `Counter`#".to_string(),
            kind: Some(ScipSymbolKind::Struct),
            display_name: Some("Counter".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let value_field = ScipSymbol {
            symbol: field_symbol.clone(),
            kind: Some(ScipSymbolKind::Field),
            display_name: Some("value".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };

        let writer_sym = ScipSymbol {
            symbol: "scip-rust pkg src/writer.rs `bump`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("bump".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let reader_sym = ScipSymbol {
            symbol: "scip-rust pkg src/reader.rs `peek`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("peek".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };

        // Helper: build an occurrence with explicit roles, since the
        // existing `definition_occurrence` / `reference_occurrence` helpers
        // hardcode their role sets.
        fn role_occurrence(symbol: &str, roles: BTreeSet<ScipSymbolRole>) -> ScipOccurrence {
            ScipOccurrence {
                symbol: symbol.to_string(),
                range: ScipRange {
                    start_line: 1,
                    start_character: 4,
                    end_line: 1,
                    end_character: 10,
                },
                enclosing_range: None,
                roles,
                syntax_kind: None,
                override_documentation: vec![],
            }
        }

        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/counter.rs"),
                    definitions: vec![
                        definition_occurrence(&counter_struct.symbol),
                        definition_occurrence(&field_symbol),
                    ],
                    references: vec![],
                    occurrences: vec![
                        definition_occurrence(&counter_struct.symbol),
                        definition_occurrence(&field_symbol),
                    ],
                    symbols: vec![counter_struct, value_field],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/writer.rs"),
                    definitions: vec![definition_occurrence(&writer_sym.symbol)],
                    references: vec![role_occurrence(
                        &field_symbol,
                        BTreeSet::from([ScipSymbolRole::WriteAccess]),
                    )],
                    occurrences: vec![
                        definition_occurrence(&writer_sym.symbol),
                        role_occurrence(
                            &field_symbol,
                            BTreeSet::from([ScipSymbolRole::WriteAccess]),
                        ),
                    ],
                    symbols: vec![writer_sym],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/reader.rs"),
                    definitions: vec![definition_occurrence(&reader_sym.symbol)],
                    references: vec![role_occurrence(
                        &field_symbol,
                        BTreeSet::from([ScipSymbolRole::ReadAccess]),
                    )],
                    occurrences: vec![
                        definition_occurrence(&reader_sym.symbol),
                        role_occurrence(
                            &field_symbol,
                            BTreeSet::from([ScipSymbolRole::ReadAccess]),
                        ),
                    ],
                    symbols: vec![reader_sym],
                },
            ],
            external_symbols: vec![],
        };

        let graph = RepoDependencyGraph::build(&[index]);

        // The role-aware classification fires on the symbol→target_file
        // edge minted in `add_reference`. Locate the field-defining file
        // and the writer/reader source files.
        let counter_file = graph
            .file_node("src/counter.rs")
            .expect("counter file node");
        let writer_file = graph
            .file_node("src/writer.rs")
            .expect("writer file node");
        let reader_file = graph
            .file_node("src/reader.rs")
            .expect("reader file node");
        let field_node = graph.symbol_node(&field_symbol).expect("field node");

        // From the WRITE site, the field's symbol→counter_file edge should
        // be tagged `Writes`. The field has *both* a writer site and a
        // reader site, so to attribute the right kind to the right source
        // we sweep edges *out of the field node* — there is one
        // SymbolReference-class edge per occurrence kind into
        // `src/counter.rs`. We expect exactly one `Writes` and one `Reads`
        // edge from the field node into the counter file.
        let mut writes_count = 0;
        let mut reads_count = 0;
        let mut bare_ref_count = 0;
        for edge in graph
            .graph()
            .edges_directed(field_node, petgraph::Direction::Outgoing)
        {
            if edge.target() != counter_file {
                continue;
            }
            match edge.weight().kind {
                RepoGraphEdgeKind::Writes => writes_count += 1,
                RepoGraphEdgeKind::Reads => reads_count += 1,
                RepoGraphEdgeKind::SymbolReference => bare_ref_count += 1,
                _ => {}
            }
        }
        assert_eq!(
            writes_count, 1,
            "expected exactly one Writes edge from field to counter file"
        );
        assert_eq!(
            reads_count, 1,
            "expected exactly one Reads edge from field to counter file"
        );
        assert_eq!(
            bare_ref_count, 0,
            "no bare SymbolReference edge should remain after the split when SCIP roles are populated"
        );

        // And the confidence floors land on the values pinned in this PR.
        for edge in graph.graph().edge_references() {
            let edge = edge.weight();
            match edge.kind {
                RepoGraphEdgeKind::Writes => {
                    assert!(
                        (edge.confidence - EDGE_CONFIDENCE_WRITES).abs() < 1e-9,
                        "Writes edge confidence {} != floor {}",
                        edge.confidence,
                        EDGE_CONFIDENCE_WRITES
                    );
                }
                RepoGraphEdgeKind::Reads => {
                    assert!(
                        (edge.confidence - EDGE_CONFIDENCE_READS).abs() < 1e-9,
                        "Reads edge confidence {} != floor {}",
                        edge.confidence,
                        EDGE_CONFIDENCE_READS
                    );
                }
                _ => {}
            }
        }

        // The unused vars silence "fields never read" without dropping
        // the assertions.
        let _ = (writer_file, reader_file);
    }

    /// `kind_filter=writes` on the field's neighbors picks out the file
    /// that *writes* it; `reads` picks out the *reader*. This is the
    /// acceptance criterion in the PR A3 plan: "fixture project with a
    /// struct field; assert `kind_filter=writes` returns only writers,
    /// `reads` only readers".
    ///
    /// The `neighbors` op itself lives in the bridge; here we exercise the
    /// underlying graph the bridge filters over.
    #[test]
    fn neighbors_kind_filter_writes_returns_only_writers_pr_a3() {
        // Reuse the multi-file fixture set up by the previous test by
        // inlining a smaller variant focused on just the field node.
        let field_symbol = "scip-rust pkg src/counter.rs `f`#`v`.".to_string();
        let value_field = ScipSymbol {
            symbol: field_symbol.clone(),
            kind: Some(ScipSymbolKind::Field),
            display_name: Some("v".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        fn role_occurrence(symbol: &str, roles: BTreeSet<ScipSymbolRole>) -> ScipOccurrence {
            ScipOccurrence {
                symbol: symbol.to_string(),
                range: ScipRange {
                    start_line: 1,
                    start_character: 4,
                    end_line: 1,
                    end_character: 10,
                },
                enclosing_range: None,
                roles,
                syntax_kind: None,
                override_documentation: vec![],
            }
        }

        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/counter.rs"),
                    definitions: vec![definition_occurrence(&field_symbol)],
                    references: vec![],
                    occurrences: vec![definition_occurrence(&field_symbol)],
                    symbols: vec![value_field],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/writer.rs"),
                    definitions: vec![],
                    references: vec![role_occurrence(
                        &field_symbol,
                        BTreeSet::from([ScipSymbolRole::WriteAccess]),
                    )],
                    occurrences: vec![role_occurrence(
                        &field_symbol,
                        BTreeSet::from([ScipSymbolRole::WriteAccess]),
                    )],
                    symbols: vec![],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/reader.rs"),
                    definitions: vec![],
                    references: vec![role_occurrence(
                        &field_symbol,
                        BTreeSet::from([ScipSymbolRole::ReadAccess]),
                    )],
                    occurrences: vec![role_occurrence(
                        &field_symbol,
                        BTreeSet::from([ScipSymbolRole::ReadAccess]),
                    )],
                    symbols: vec![],
                },
            ],
            external_symbols: vec![],
        };

        let graph = RepoDependencyGraph::build(&[index]);
        let counter_file = graph
            .file_node("src/counter.rs")
            .expect("counter file node");

        // Mirror the bridge-side filter: walk outgoing edges from the
        // field node into `src/counter.rs` and partition by edge kind.
        let field_node = graph.symbol_node(&field_symbol).expect("field node");
        let writes: Vec<_> = graph
            .graph()
            .edges_directed(field_node, petgraph::Direction::Outgoing)
            .filter(|e| {
                e.target() == counter_file && e.weight().kind == RepoGraphEdgeKind::Writes
            })
            .collect();
        let reads: Vec<_> = graph
            .graph()
            .edges_directed(field_node, petgraph::Direction::Outgoing)
            .filter(|e| e.target() == counter_file && e.weight().kind == RepoGraphEdgeKind::Reads)
            .collect();

        assert_eq!(writes.len(), 1, "expected one Writes edge");
        assert_eq!(reads.len(), 1, "expected one Reads edge");

        // Confidence floors propagate from the table in this module.
        assert!(
            (writes[0].weight().confidence - EDGE_CONFIDENCE_WRITES).abs() < 1e-9,
            "Writes confidence floor mismatch"
        );
        assert!(
            (reads[0].weight().confidence - EDGE_CONFIDENCE_READS).abs() < 1e-9,
            "Reads confidence floor mismatch"
        );
    }

    // ---- patch_changed_files tests ----

    /// Build a modified index where src/app.rs has a new symbol and a removed
    /// reference, then patch the original graph and verify the result reflects
    /// the changes.
    #[test]
    fn patch_changed_files_updates_graph_for_modified_file() {
        let original = RepoDependencyGraph::build(&[fixture_index()]);

        // The original graph has src/helper.rs and src/app.rs.
        assert!(original.file_node("src/app.rs").is_some());
        assert!(original.file_node("src/helper.rs").is_some());
        assert!(
            original
                .symbol_node("scip-rust pkg src/app.rs `main`().")
                .is_some()
        );

        // Build a replacement for src/app.rs that has a new symbol "run"
        // instead of "main" and no reference to helper.
        let run_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/app.rs `run`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("run".to_string()),
            signature: Some("fn run()".to_string()),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let new_index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/app.rs"),
                definitions: vec![definition_occurrence(&run_symbol.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&run_symbol.symbol)],
                symbols: vec![run_symbol],
            }],
            external_symbols: vec![],
        };

        let changed = BTreeSet::from([PathBuf::from("src/app.rs")]);
        let patched = original.patch_changed_files(&changed, &[new_index]);

        // The old "main" symbol should be gone; the new "run" symbol should exist.
        assert!(
            patched
                .symbol_node("scip-rust pkg src/app.rs `main`().")
                .is_none(),
            "old main symbol should be removed after patch"
        );
        assert!(
            patched
                .symbol_node("scip-rust pkg src/app.rs `run`().")
                .is_some(),
            "new run symbol should be present after patch"
        );

        // src/helper.rs and its symbol should be untouched.
        assert!(patched.file_node("src/helper.rs").is_some());
        assert!(
            patched
                .symbol_node("scip-rust pkg src/helper.rs `helper`().")
                .is_some()
        );

        // src/app.rs file node should still exist (re-added by the new index).
        assert!(patched.file_node("src/app.rs").is_some());

        // Ranking should still work and produce valid output.
        let patched_ranking = patched.rank();
        assert!(!patched_ranking.nodes.is_empty());

        // The helper symbol should still rank high (it was not changed).
        let helper_rank = patched_ranking
            .nodes
            .iter()
            .position(|n| {
                n.key == RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string())
            })
            .expect("helper should be ranked");
        assert!(helper_rank < patched_ranking.nodes.len());
    }

    /// Patching with an empty changed-file set produces the same graph.
    #[test]
    fn patch_with_no_changed_files_preserves_graph() {
        let original = RepoDependencyGraph::build(&[fixture_index()]);
        let changed: BTreeSet<PathBuf> = BTreeSet::new();
        let patched = original.patch_changed_files(&changed, &[]);
        assert_eq!(patched.node_count(), original.node_count());
        assert_eq!(patched.edge_count(), original.edge_count());
    }

    /// Patching a file that does not exist in the graph is a no-op for removal
    /// and just adds new data.
    #[test]
    fn patch_nonexistent_file_adds_new_data() {
        let original = RepoDependencyGraph::build(&[fixture_index()]);
        let original_node_count = original.node_count();

        let new_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/new.rs `new_fn`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("new_fn".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let new_index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/new.rs"),
                definitions: vec![definition_occurrence(&new_symbol.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&new_symbol.symbol)],
                symbols: vec![new_symbol],
            }],
            external_symbols: vec![],
        };

        let changed = BTreeSet::from([PathBuf::from("src/new.rs")]);
        let patched = original.patch_changed_files(&changed, &[new_index]);

        // New file and symbol added.
        assert!(patched.file_node("src/new.rs").is_some());
        assert!(
            patched
                .symbol_node("scip-rust pkg src/new.rs `new_fn`().")
                .is_some()
        );
        // Original nodes preserved.
        assert!(patched.node_count() > original_node_count);
        assert!(patched.file_node("src/app.rs").is_some());
        assert!(patched.file_node("src/helper.rs").is_some());
    }


    // ── Chunk B: search / cycles / orphans / path tests ─────────────────────

    #[test]
    fn search_by_name_finds_substring_and_ranks_exact_first() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let hits = graph.search_by_name("helper", None, 10);
        assert!(!hits.is_empty(), "expected at least one hit for 'helper'");
        // The exact-name hit (display_name = "helper") should be first.
        let first = &graph.node(hits[0].node_index).display_name;
        assert_eq!(first.to_lowercase(), "helper");
    }

    #[test]
    fn search_by_name_respects_kind_filter() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let hits = graph.search_by_name("helper", Some(RepoGraphNodeKind::Symbol), 10);
        for hit in &hits {
            assert_eq!(graph.node(hit.node_index).kind, RepoGraphNodeKind::Symbol);
        }
    }

    #[test]
    fn cycles_finds_symbol_cycle_via_relationships() {
        // Two mutually-referencing symbols via SCIP relationships create a
        // symbol-level cycle that tarjan_scc must report.
        let a_sym = ScipSymbol {
            symbol: "scip-rust pkg src/a.rs `a_fn`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("a_fn".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![ScipRelationship {
                source_symbol: "scip-rust pkg src/a.rs `a_fn`().".to_string(),
                target_symbol: "scip-rust pkg src/b.rs `b_fn`().".to_string(),
                kinds: BTreeSet::from([ScipRelationshipKind::Reference]),
            }],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let b_sym = ScipSymbol {
            symbol: "scip-rust pkg src/b.rs `b_fn`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("b_fn".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![ScipRelationship {
                source_symbol: "scip-rust pkg src/b.rs `b_fn`().".to_string(),
                target_symbol: "scip-rust pkg src/a.rs `a_fn`().".to_string(),
                kinds: BTreeSet::from([ScipRelationshipKind::Reference]),
            }],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/a.rs"),
                    definitions: vec![definition_occurrence(&a_sym.symbol)],
                    references: vec![],
                    occurrences: vec![definition_occurrence(&a_sym.symbol)],
                    symbols: vec![a_sym.clone()],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/b.rs"),
                    definitions: vec![definition_occurrence(&b_sym.symbol)],
                    references: vec![],
                    occurrences: vec![definition_occurrence(&b_sym.symbol)],
                    symbols: vec![b_sym.clone()],
                },
            ],
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[index]);
        let sccs = graph.strongly_connected_components(Some(RepoGraphNodeKind::Symbol), 2);
        let has_two_symbol_cycle = sccs.iter().any(|component| {
            component.len() >= 2
                && component
                    .iter()
                    .all(|n| graph.node(*n).kind == RepoGraphNodeKind::Symbol)
        });
        assert!(
            has_two_symbol_cycle,
            "expected a symbol-level cycle of size >= 2; got SCCs: {sccs:?}"
        );
    }

    #[test]
    fn orphans_filters_by_visibility() {
        let public_unused = ScipSymbol {
            symbol: "scip-rust pkg src/lib.rs `PublicUnused`#".to_string(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some("PublicUnused".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let private_unused = ScipSymbol {
            symbol: "local 1".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("private_unused".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Private),
        signature_parts: None,
        };
        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/lib.rs"),
                definitions: vec![
                    definition_occurrence(&public_unused.symbol),
                    definition_occurrence(&private_unused.symbol),
                ],
                references: vec![],
                occurrences: vec![
                    definition_occurrence(&public_unused.symbol),
                    definition_occurrence(&private_unused.symbol),
                ],
                symbols: vec![public_unused.clone(), private_unused.clone()],
            }],
            external_symbols: vec![],
        };
        let graph = RepoDependencyGraph::build(&[index]);

        let public_orphans = graph.orphans(
            Some(RepoGraphNodeKind::Symbol),
            Some(crate::scip_parser::ScipVisibility::Public),
            100,
        );
        let public_names: Vec<String> = public_orphans
            .iter()
            .map(|idx| graph.node(*idx).display_name.clone())
            .collect();
        assert!(public_names.iter().any(|n| n == "PublicUnused"));
        assert!(!public_names.iter().any(|n| n == "private_unused"));

        let private_orphans = graph.orphans(
            Some(RepoGraphNodeKind::Symbol),
            Some(crate::scip_parser::ScipVisibility::Private),
            100,
        );
        let private_names: Vec<String> = private_orphans
            .iter()
            .map(|idx| graph.node(*idx).display_name.clone())
            .collect();
        assert!(private_names.iter().any(|n| n == "private_unused"));
        assert!(!private_names.iter().any(|n| n == "PublicUnused"));
    }

    #[test]
    fn shortest_path_finds_route_between_two_nodes() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let from = graph.file_node("src/app.rs").expect("app file");
        let to = graph
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .expect("helper symbol");
        let path = graph
            .shortest_path(from, to, None)
            .expect("there should be a path from app to helper");
        assert!(path.len() >= 2);
        assert_eq!(path[0], from);
        assert_eq!(*path.last().unwrap(), to);
    }

    // ── symbols_enclosing tests ─────────────────────────────────────────────

    /// Build a SCIP occurrence for a definition with an explicit enclosing
    /// range (0-indexed, half-open-like on the wire; `symbols_enclosing`
    /// normalizes to 1-indexed inclusive).
    fn definition_with_enclosing(
        symbol: &str,
        enclosing_start: i32,
        enclosing_end: i32,
    ) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: enclosing_start,
                start_character: 0,
                end_line: enclosing_start,
                end_character: 6,
            },
            enclosing_range: Some(ScipRange {
                start_line: enclosing_start,
                start_character: 0,
                end_line: enclosing_end,
                end_character: 0,
            }),
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    /// Fixture with nested symbols in one file and a separate sibling file:
    ///
    /// `src/lib.rs`:
    /// - `outer` module, lines 1..20 (0-indexed: 0..=19)
    /// - `Inner` struct, lines 5..12 (nested in outer)
    /// - `inner_method` method, lines 7..10 (nested in Inner)
    /// - `sibling_fn` function, lines 25..30 (sibling of outer)
    ///
    /// `src/other.rs`:
    /// - `other_fn` function, lines 1..5
    fn nested_ranges_fixture() -> ParsedScipIndex {
        let outer_sym = ScipSymbol {
            symbol: "scip-rust pkg src/lib.rs `outer`/".to_string(),
            kind: Some(ScipSymbolKind::Namespace),
            display_name: Some("outer".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let inner_sym = ScipSymbol {
            symbol: "scip-rust pkg src/lib.rs `outer`/`Inner`#".to_string(),
            kind: Some(ScipSymbolKind::Struct),
            display_name: Some("Inner".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let method_sym = ScipSymbol {
            symbol: "scip-rust pkg src/lib.rs `outer`/`Inner`#`inner_method`().".to_string(),
            kind: Some(ScipSymbolKind::Method),
            display_name: Some("inner_method".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let sibling_sym = ScipSymbol {
            symbol: "scip-rust pkg src/lib.rs `sibling_fn`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("sibling_fn".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };
        let other_sym = ScipSymbol {
            symbol: "scip-rust pkg src/other.rs `other_fn`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("other_fn".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
        };

        ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/lib.rs"),
                    // 0-indexed on the wire; the normalization adds 1 so the
                    // resulting 1-indexed inclusive ranges are:
                    //   outer:        1..=20
                    //   Inner:        6..=12
                    //   inner_method: 8..=10
                    //   sibling_fn:  26..=30
                    definitions: vec![
                        definition_with_enclosing(&outer_sym.symbol, 0, 19),
                        definition_with_enclosing(&inner_sym.symbol, 5, 11),
                        definition_with_enclosing(&method_sym.symbol, 7, 9),
                        definition_with_enclosing(&sibling_sym.symbol, 25, 29),
                    ],
                    references: vec![],
                    occurrences: vec![],
                    symbols: vec![
                        outer_sym.clone(),
                        inner_sym.clone(),
                        method_sym.clone(),
                        sibling_sym.clone(),
                    ],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/other.rs"),
                    // 1..=5 after normalization.
                    definitions: vec![definition_with_enclosing(&other_sym.symbol, 0, 4)],
                    references: vec![],
                    occurrences: vec![],
                    symbols: vec![other_sym.clone()],
                },
            ],
            external_symbols: vec![],
        }
    }

    #[test]
    fn symbols_enclosing_range_inside_single_symbol_returns_only_that_symbol() {
        let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
        // Lines 27..=28 fall wholly inside `sibling_fn` (26..=30).
        let hits = graph.symbols_enclosing(Path::new("src/lib.rs"), 27, 28);
        let names: Vec<_> = hits
            .iter()
            .map(|idx| graph.node(*idx).display_name.as_str())
            .collect();
        assert_eq!(names, vec!["sibling_fn"]);
    }

    #[test]
    fn symbols_enclosing_range_crossing_sibling_symbols_returns_both() {
        let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
        // Lines 20..=26 span the gap between `outer` (1..=20) and
        // `sibling_fn` (26..=30); both overlap at their boundary lines.
        let hits = graph.symbols_enclosing(Path::new("src/lib.rs"), 20, 26);
        let mut names: Vec<_> = hits
            .iter()
            .map(|idx| graph.node(*idx).display_name.clone())
            .collect();
        names.sort();
        assert_eq!(names, vec!["outer".to_string(), "sibling_fn".to_string()]);
    }

    #[test]
    fn symbols_enclosing_nested_symbols_all_enclose_query() {
        let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
        // Line 9 sits inside `inner_method` (8..=10), which is inside
        // `Inner` (6..=12), which is inside `outer` (1..=20).
        let hits = graph.symbols_enclosing(Path::new("src/lib.rs"), 9, 9);
        let mut names: Vec<_> = hits
            .iter()
            .map(|idx| graph.node(*idx).display_name.clone())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Inner".to_string(),
                "inner_method".to_string(),
                "outer".to_string()
            ]
        );
    }

    #[test]
    fn symbols_enclosing_file_without_ranges_returns_empty() {
        // The base fixture uses definition_occurrence() which has
        // enclosing_range=None, so symbol_ranges is empty for that file.
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let hits = graph.symbols_enclosing(Path::new("src/app.rs"), 1, 100);
        assert!(hits.is_empty());
    }

    #[test]
    fn symbols_enclosing_unknown_file_returns_empty() {
        let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
        let hits = graph.symbols_enclosing(Path::new("src/does_not_exist.rs"), 1, 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn symbols_enclosing_round_trips_through_artifact() {
        // PR A1: `symbol_ranges` must be persisted in the artifact so that
        // `code_graph symbols_at` / `diff_touches` keep working after a
        // cache-hit reload (DB-restored graph).
        let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
        let baseline = graph.symbols_enclosing(Path::new("src/lib.rs"), 9, 9);
        assert!(
            !baseline.is_empty(),
            "fixture must produce ranges before round-trip"
        );

        let artifact = graph.to_artifact();
        assert!(
            !artifact.symbol_ranges.is_empty(),
            "artifact must carry symbol_ranges"
        );
        let restored = RepoDependencyGraph::from_artifact(&artifact);

        let hits = restored.symbols_enclosing(Path::new("src/lib.rs"), 9, 9);
        assert!(
            !hits.is_empty(),
            "artifact-restored graph must preserve enclosing-range hits"
        );

        let mut names: Vec<_> = hits
            .iter()
            .map(|idx| restored.node(*idx).display_name.clone())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Inner".to_string(),
                "inner_method".to_string(),
                "outer".to_string()
            ],
            "restored ranges must match the freshly-built graph's hits"
        );
    }

    #[test]
    fn symbol_ranges_round_trip_through_json_artifact() {
        // Belt-and-suspenders coverage of the JSON path used by
        // `serialize_artifact` / `deserialize_artifact`, which is what the
        // cache-hit reload exercises end-to-end.
        let graph = RepoDependencyGraph::build(&[nested_ranges_fixture()]);
        let json = graph.serialize_artifact().expect("serialize");
        let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize");
        let hits = restored.symbols_enclosing(Path::new("src/other.rs"), 1, 5);
        assert!(
            !hits.is_empty(),
            "JSON-round-tripped graph must preserve symbol_ranges"
        );
    }

    // ── iter 26: complexity metrics post-pass ─────────────────────────────

    /// Build a Rust SCIP fixture with two function symbols whose enclosing
    /// ranges line up with `source` so the post-build complexity walker
    /// can pair them by overlap.
    fn complexity_fixture(source: &str) -> ParsedScipIndex {
        // Both `simple` and `nested` start at the top of file in `source`;
        // we hard-code the ranges to match the literal layout below.
        // `simple`: lines 1..=4 (1-indexed inclusive after normalization),
        // `nested`: lines 6..=15.
        let simple_sym = ScipSymbol {
            symbol: "scip-rust pkg src/lib.rs `simple`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("simple".to_string()),
            signature: Some("fn simple()".to_string()),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
            signature_parts: None,
        };
        let nested_sym = ScipSymbol {
            symbol: "scip-rust pkg src/lib.rs `nested`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("nested".to_string()),
            signature: Some("fn nested(a: i32, b: i32)".to_string()),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
            signature_parts: None,
        };

        // `definition_with_enclosing` takes 0-indexed wire ranges and the
        // builder bumps them to 1-indexed inclusive — see
        // `record_symbol_range`. So a fn whose body spans 1-indexed
        // inclusive lines [1,4] is encoded as (0, 3) here.
        let _ = source; // silence unused if the layout drifts; lines pinned below.

        ParsedScipIndex {
            metadata: ScipMetadata {
                project_root: Some("file:///workspace/repo".to_string()),
                tool_name: Some("scip-rust".to_string()),
                tool_version: Some("test".to_string()),
            },
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/lib.rs"),
                definitions: vec![
                    definition_with_enclosing(&simple_sym.symbol, 0, 2),
                    definition_with_enclosing(&nested_sym.symbol, 4, 13),
                ],
                references: vec![],
                occurrences: vec![],
                symbols: vec![simple_sym, nested_sym],
            }],
            external_symbols: vec![],
        }
    }

    /// Source whose tree-sitter ranges align with `complexity_fixture`:
    ///   `simple`: 0-indexed rows 0..=2  (1-indexed 1..=3)
    ///   `nested`: 0-indexed rows 4..=13 (1-indexed 5..=14)
    /// The body of `nested` carries an `if` inside an `if` inside a `for`
    /// inside an `if`: cognitive = 1 + 2 + 3 + 4 = 10
    /// (matches `complexity::tests::deeply_nested_chains_correctly`).
    const COMPLEXITY_FIXTURE_SOURCE: &str = "fn simple() {\n    let _ = 1;\n}\n\nfn nested(a: i32, b: i32) {\n    if a > 0 {\n        if b > 0 {\n            for _ in 0..a {\n                if b == 1 {\n                }\n            }\n        }\n    }\n}\n";

    #[test]
    fn build_with_source_attaches_complexity_to_function_nodes() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let abs = tempdir.path().join("src/lib.rs");
        std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
        std::fs::write(&abs, COMPLEXITY_FIXTURE_SOURCE).expect("write");

        let graph = RepoDependencyGraph::build_with_source(
            &[complexity_fixture(COMPLEXITY_FIXTURE_SOURCE)],
            Some(tempdir.path()),
        );

        let simple_idx = graph
            .symbol_node("scip-rust pkg src/lib.rs `simple`().")
            .expect("simple node");
        let nested_idx = graph
            .symbol_node("scip-rust pkg src/lib.rs `nested`().")
            .expect("nested node");

        let simple = graph.node(simple_idx);
        let nested = graph.node(nested_idx);

        let simple_metrics = simple.complexity.expect("simple has metrics");
        assert_eq!(simple_metrics.cyclomatic, 1);
        assert_eq!(simple_metrics.cognitive, 0);

        let nested_metrics = nested.complexity.expect("nested has metrics");
        assert_eq!(nested_metrics.cognitive, 1 + 2 + 3 + 4);
        assert_eq!(nested_metrics.param_count, 2);
        assert_eq!(nested_metrics.max_nesting, 4);
    }

    #[test]
    fn build_without_source_leaves_complexity_unset() {
        // `build` (no project_root) is the synthetic-fixture path used by
        // most unit tests in this file. The post-pass must short-circuit
        // gracefully — no panic, no metrics, just `None` everywhere.
        let graph = RepoDependencyGraph::build(&[complexity_fixture(
            COMPLEXITY_FIXTURE_SOURCE,
        )]);
        let simple_idx = graph
            .symbol_node("scip-rust pkg src/lib.rs `simple`().")
            .expect("simple node");
        assert!(graph.node(simple_idx).complexity.is_none());
    }

    #[test]
    fn complexity_round_trips_through_artifact() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let abs = tempdir.path().join("src/lib.rs");
        std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
        std::fs::write(&abs, COMPLEXITY_FIXTURE_SOURCE).expect("write");

        let graph = RepoDependencyGraph::build_with_source(
            &[complexity_fixture(COMPLEXITY_FIXTURE_SOURCE)],
            Some(tempdir.path()),
        );

        let artifact = graph.to_artifact();
        assert_eq!(artifact.version, REPO_GRAPH_ARTIFACT_VERSION);

        let restored = RepoDependencyGraph::from_artifact(&artifact);
        let nested_idx = restored
            .symbol_node("scip-rust pkg src/lib.rs `nested`().")
            .expect("nested node restored");
        let metrics = restored
            .node(nested_idx)
            .complexity
            .expect("metrics survive round-trip");
        assert_eq!(metrics.cognitive, 1 + 2 + 3 + 4);
        assert_eq!(metrics.param_count, 2);
    }

    #[test]
    fn complexity_skips_files_with_unsupported_language() {
        // SCIP `Document.language` strings outside the walker's table
        // (iter 23–25 ships 11 languages — anything else, like
        // "haskell", falls through `ComplexityLang::from_scip`). The
        // post-pass must skip silently rather than panic, leaving
        // `complexity = None` on every node.
        let hs_source = "module M where\nf :: Int\nf = 0\n";
        let tempdir = tempfile::tempdir().expect("tempdir");
        let abs = tempdir.path().join("src/M.hs");
        std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
        std::fs::write(&abs, hs_source).expect("write");

        let hs_sym = ScipSymbol {
            symbol: "scip-haskell pkg src/M.hs `f`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("f".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
            signature_parts: None,
        };
        let index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "haskell".to_string(),
                relative_path: PathBuf::from("src/M.hs"),
                definitions: vec![definition_with_enclosing(&hs_sym.symbol, 2, 3)],
                references: vec![],
                occurrences: vec![],
                symbols: vec![hs_sym],
            }],
            external_symbols: vec![],
        };

        let graph = RepoDependencyGraph::build_with_source(&[index], Some(tempdir.path()));
        let f_idx = graph
            .symbol_node("scip-haskell pkg src/M.hs `f`().")
            .expect("haskell fn node");
        assert!(graph.node(f_idx).complexity.is_none());
    }
}
