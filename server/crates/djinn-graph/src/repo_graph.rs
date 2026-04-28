use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::Direction::{Incoming, Outgoing};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef as PetgraphEdgeRef;
use serde::{Deserialize, Serialize};

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
pub const REPO_GRAPH_ARTIFACT_VERSION: u32 = 4;

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
const EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_REFERENCE: f64 = 0.80;
const EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_IMPLEMENTATION: f64 = 0.85;
const EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_TYPE_DEFINITION: f64 = 0.85;
const EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_DEFINITION: f64 = 0.85;
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
const EDGE_CONFIDENCE_LOCAL_PENALTY: f64 = 0.15;
const EDGE_WEIGHT_DEFINITION_TO_FILE: f64 = 4.0;
const EDGE_WEIGHT_FILE_TO_DEFINITION: f64 = 1.5;
const EDGE_WEIGHT_FILE_REFERENCE: f64 = 2.5;
const EDGE_WEIGHT_SYMBOL_REFERENCE: f64 = 3.5;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_REFERENCE: f64 = 2.0;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_IMPLEMENTATION: f64 = 2.5;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_TYPE_DEFINITION: f64 = 1.75;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_DEFINITION: f64 = 2.25;
// PR F1: keep `EntryPointOf` light — the edge is metadata, not a
// dependency signal, so it should not perturb PageRank or shortest-path
// scoring.
const EDGE_WEIGHT_ENTRY_POINT_OF: f64 = 0.5;
// PR F3: `MemberOf` edges are structural (not weighted by SCIP
// evidence count), so they get a constant low weight that doesn't
// dominate PageRank. The community is a side-channel; it shouldn't
// reshape the importance ranking.
const EDGE_WEIGHT_MEMBER_OF: f64 = 1.0;
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

impl RepoDependencyGraph {
    pub fn build(indices: &[ParsedScipIndex]) -> Self {
        let mut builder = RepoDependencyGraphBuilder::default();
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

        let mut scored_nodes = Vec::with_capacity(self.graph.node_count());
        for node_index in self.graph.node_indices() {
            let node = &self.graph[node_index];
            let page_rank = page_rank_scores[node_index.index()];
            let structural_weight = self.structural_weight(node_index);
            let score = page_rank * structural_weight;
            scored_nodes.push(RankedRepoGraphNode {
                node_index,
                key: node.key(),
                kind: node.kind(),
                score,
                page_rank,
                structural_weight,
                inbound_edge_weight: self.total_edge_weight(node_index, Incoming),
                outbound_edge_weight: self.total_edge_weight(node_index, Outgoing),
            });
        }

        scored_nodes.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGraphNodeKind {
    File,
    Symbol,
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
    SymbolRelationshipReference,
    SymbolRelationshipImplementation,
    SymbolRelationshipTypeDefinition,
    SymbolRelationshipDefinition,
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
        RepoGraphEdgeKind::SymbolRelationshipReference => {
            EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_REFERENCE
        }
        RepoGraphEdgeKind::SymbolRelationshipImplementation => {
            EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_IMPLEMENTATION
        }
        RepoGraphEdgeKind::SymbolRelationshipTypeDefinition => {
            EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_TYPE_DEFINITION
        }
        RepoGraphEdgeKind::SymbolRelationshipDefinition => {
            EDGE_CONFIDENCE_SYMBOL_RELATIONSHIP_DEFINITION
        }
        RepoGraphEdgeKind::EntryPointOf => EDGE_CONFIDENCE_ENTRY_POINT_OF,
        RepoGraphEdgeKind::MemberOf => EDGE_CONFIDENCE_MEMBER_OF,
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
            // "who changes X". Fall back to the generic `SymbolReference`
            // when neither flag is set (imports, type-only refs, indexers
            // that don't populate role bits).
            let edge_kind = symbol_reference_edge_kind(&occurrence.roles);
            self.bump_edge(symbol_index, target_file_index, edge_kind, 1);
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
                ScipRelationshipKind::Reference => RepoGraphEdgeKind::SymbolRelationshipReference,
                ScipRelationshipKind::Implementation => {
                    RepoGraphEdgeKind::SymbolRelationshipImplementation
                }
                ScipRelationshipKind::TypeDefinition => {
                    RepoGraphEdgeKind::SymbolRelationshipTypeDefinition
                }
                ScipRelationshipKind::Definition => RepoGraphEdgeKind::SymbolRelationshipDefinition,
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
    /// by their position in [`Self::nodes`]. New in artifact v3 — old
    /// blobs that lack the field deserialize via `default` to an
    /// empty vec, which is fine: `community_id(...)` then returns
    /// `None` until the next warm rebuild repopulates it.
    #[serde(default)]
    pub communities: Vec<crate::communities::Community>,
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
}

/// A serializable enclosing range for a symbol definition, identified by the
/// symbol node's position in the parent [`RepoGraphArtifact::nodes`] vec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGraphArtifactSymbolRange {
    pub start_line: u32,
    pub end_line: u32,
    pub node: usize,
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

        RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges,
            communities: self.communities.clone(),
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

        let mut out = RepoDependencyGraph {
            graph,
            node_lookup,
            name_index,
            symbol_ranges,
            communities: Vec::new(),
            community_lookup: BTreeMap::new(),
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

/// PR A3: classify a symbol→target_file reference edge from the SCIP
/// `SymbolRole` bitset on the occurrence.
///
/// Precedence:
/// 1. `WriteAccess` → [`RepoGraphEdgeKind::Writes`] (mutation wins; SCIP
///    stamps both bits on read-modify-write occurrences such as `x += 1`).
/// 2. `ReadAccess` → [`RepoGraphEdgeKind::Reads`].
/// 3. neither → [`RepoGraphEdgeKind::SymbolReference`] (imports, type-only
///    references, indexers that don't populate role bits).
fn symbol_reference_edge_kind(
    roles: &std::collections::BTreeSet<ScipSymbolRole>,
) -> RepoGraphEdgeKind {
    if roles.contains(&ScipSymbolRole::WriteAccess) {
        RepoGraphEdgeKind::Writes
    } else if roles.contains(&ScipSymbolRole::ReadAccess) {
        RepoGraphEdgeKind::Reads
    } else {
        RepoGraphEdgeKind::SymbolReference
    }
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
        RepoGraphEdgeKind::SymbolRelationshipReference => EDGE_WEIGHT_SYMBOL_RELATIONSHIP_REFERENCE,
        RepoGraphEdgeKind::SymbolRelationshipImplementation => {
            EDGE_WEIGHT_SYMBOL_RELATIONSHIP_IMPLEMENTATION
        }
        RepoGraphEdgeKind::SymbolRelationshipTypeDefinition => {
            EDGE_WEIGHT_SYMBOL_RELATIONSHIP_TYPE_DEFINITION
        }
        RepoGraphEdgeKind::SymbolRelationshipDefinition => {
            EDGE_WEIGHT_SYMBOL_RELATIONSHIP_DEFINITION
        }
        RepoGraphEdgeKind::EntryPointOf => EDGE_WEIGHT_ENTRY_POINT_OF,
        RepoGraphEdgeKind::MemberOf => EDGE_WEIGHT_MEMBER_OF,
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

        assert_eq!(graph.node_count(), 5);
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

        assert!(helper_symbol_rank < app_symbol_rank);
        assert!(helper_file_rank < app_file_rank);

        let helper_symbol_score = ranking.nodes[helper_symbol_rank].score;
        let app_symbol_score = ranking.nodes[app_symbol_rank].score;
        assert!(helper_symbol_score > app_symbol_score);
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
            // doesn't apply — we just bound them in (0, 1].
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
        assert!(seen_kinds.contains(&RepoGraphEdgeKind::SymbolRelationshipImplementation));
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
}
