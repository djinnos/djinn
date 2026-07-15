// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.

//! `RepoDependencyGraph` — the in-memory petgraph-backed repo graph
//! data structure plus its query/lookup/neighborhood helpers.
//!
//! The builder that produces the graph lives in
//! [`super::builder`] (placeholder until the `3hrr` follow-up task
//! lands), the artifact (de)serialization shape in [`super::artifact`]
//! (placeholder until `yxp7` lands), and the PageRank / RRF math in
//! [`super::ranking`]. This file owns the struct, its `impl` blocks,
//! and the free helper functions that are tightly coupled to those
//! methods (name index, process lookup, complexity attachment, edge
//! confidence derivation).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::Direction::{Incoming, Outgoing};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

use crate::complexity::ComplexityWalker;
use crate::galaxy_layout::GalaxyLayoutPosition;
use crate::layout::GraphLayoutPosition;
use crate::scip_parser::{ParsedScipIndex, ScipFile, ScipSymbol, ScipSymbolKind, ScipVisibility};

use super::artifact::RouteExclusionConfig;
use super::constants::EDGE_CONFIDENCE_LOCAL_PENALTY;
use super::edge::{RepoGraphEdge, RepoGraphEdgeKind, edge_confidence_floor, edge_weight};
use super::node::{RepoGraphNode, RepoGraphNodeKind, RepoGraphSearchHit, RepoNodeKey};

const SYNTHETIC_LABEL_ENTROPY_MERGE_THRESHOLD: f64 = 2.0;

fn label_entropy(label: &str) -> f64 {
    if label.is_empty() {
        return 0.0;
    }
    let mut counts = BTreeMap::<char, usize>::new();
    let mut total = 0usize;
    for ch in label.chars() {
        *counts.entry(ch).or_default() += 1;
        total += 1;
    }
    counts
        .values()
        .map(|count| {
            let p = (*count as f64) / (total as f64);
            -p * p.log2()
        })
        .sum()
}

fn synthetic_source_scoped_key(
    prefix: &str,
    id: &str,
    source_file: Option<&Path>,
    workspace: Option<&str>,
    low_entropy_discriminator: Option<&str>,
    unique_discriminator: usize,
) -> String {
    let mut key = id.to_string();
    if let Some(source_file) = source_file {
        key.push_str(" @ ");
        key.push_str(&source_file.display().to_string());
    } else {
        key.push_str(" @ <unresolved-source>");
    }
    if let Some(workspace) = workspace {
        key.push_str(" [workspace=");
        key.push_str(workspace);
        key.push(']');
    } else if prefix == "tool" {
        key.push_str(" [workspace=<unresolved>]");
    }

    let can_merge_same_source = source_file.is_some()
        && (prefix != "tool" || workspace.is_some())
        && label_entropy(id) >= SYNTHETIC_LABEL_ENTROPY_MERGE_THRESHOLD;
    if !can_merge_same_source {
        key.push_str(" #");
        key.push_str(low_entropy_discriminator.unwrap_or("unresolved"));
        key.push(':');
        key.push_str(&unique_discriminator.to_string());
    }
    key
}

/// Stable, reusable repository dependency graph built from normalized SCIP parse output.
#[derive(Debug, Clone)]
pub struct RepoDependencyGraph {
    // Fields are `pub(super)` so the builder / artifact code that
    // currently lives in `super::mod` (sibling tasks `3hrr` / `yxp7`)
    // can construct the struct and walk its members. Once those
    // follow-ups land, the builder / artifact modules own the
    // construction paths and the field visibility can tighten back
    // down to private.
    pub(super) graph: DiGraph<RepoGraphNode, RepoGraphEdge>,
    pub(super) node_lookup: BTreeMap<RepoNodeKey, NodeIndex>,
    /// Index from lowercased `display_name` to the nodes that use it.
    /// Populated at build time so `search` is O(log N + k).
    pub(super) name_index: BTreeMap<String, Vec<NodeIndex>>,
    /// Per-file list of symbol-definition enclosing ranges, sorted by
    /// `start_line`. Populated by [`RepoDependencyGraph::build`] from parsed
    /// SCIP input, and round-tripped through the artifact so cache-hit
    /// reloads via [`RepoDependencyGraph::from_artifact`] retain it.
    pub(super) symbol_ranges: BTreeMap<PathBuf, Vec<SymbolRange>>,
    /// PR F3: detected communities (greedy modularity over the
    /// undirected weighted projection). Populated by
    /// [`RepoDependencyGraph::build`] when `DJINN_COMMUNITY_DETECTION`
    /// is unset/true; round-tripped through the artifact so cache-hit
    /// reloads keep them.
    pub(super) communities: Vec<crate::communities::Community>,
    /// Reverse index: `NodeIndex::index()` → position in `communities`.
    /// Built whenever `communities` is set (build-time or after
    /// `from_artifact`). Singleton nodes (not in any community) are
    /// absent from the map.
    pub(super) community_lookup: BTreeMap<usize, usize>,
    /// PR F2: detected execution-flow processes traced from each
    /// entry point. Populated by [`RepoDependencyGraph::build`] when
    /// `DJINN_PROCESS_DETECTION` is unset/true; round-tripped through
    /// the artifact so cache-hit reloads keep them.
    pub(super) processes: Vec<crate::processes::Process>,
    /// Reverse index: `NodeIndex::index()` → list of positions in
    /// `processes` where the node appears as a step. Built whenever
    /// `processes` is set (build-time or after `from_artifact`). Empty
    /// for nodes that don't participate in any traced process.
    pub(super) process_lookup: BTreeMap<usize, Vec<usize>>,
    /// PR s6ch / 92z7: in-memory copy of the [`RouteExclusionConfig`]
    /// sidecar that travels with the artifact. Carrying the config on
    /// the live graph means `impact` / `api_impact` / `route_map` /
    /// `shape_check` can apply the same exclusion policy without
    /// re-fetching the artifact, and unit tests can construct a graph
    /// with a custom config in-memory.
    pub(super) route_exclusion_config: RouteExclusionConfig,
    /// Warm-time deterministic layout positions keyed by stable node UID.
    pub(super) layout_positions: BTreeMap<String, GraphLayoutPosition>,
    /// Proposal lmkv: warm-time 3D galaxy layout positions keyed by stable
    /// node UID. Populated during warm (and lazily backfilled on legacy
    /// artifact load); round-trips through the artifact.
    pub(super) galaxy_positions: BTreeMap<String, GalaxyLayoutPosition>,
    /// Proposal lmkv: per-node collapsed-edge degree keyed by stable node UID,
    /// computed alongside [`Self::galaxy_positions`].
    pub(super) galaxy_degrees: BTreeMap<String, u32>,
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

/// Temporary build toggles for ykcg extractor rollout parity tests.
///
/// This is deliberately narrow: production build paths derive these values from
/// env flags, while tests can construct the Process-disabled baseline without
/// racing on global process environment. Delete the Process fields with the
/// temporary Process dual-build seam after rollout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoGraphBuildOptions {
    pub process_detection_enabled: bool,
    pub process_parity_enabled: bool,
    pub community_crate_seeding_enabled: bool,
}

impl RepoGraphBuildOptions {
    pub fn from_env() -> Self {
        Self {
            process_detection_enabled: crate::processes::process_detection_enabled(),
            process_parity_enabled: crate::processes::process_parity_enabled(),
            community_crate_seeding_enabled: crate::communities::crate_seeding_enabled(),
        }
    }

    pub fn with_process_detection(mut self, enabled: bool) -> Self {
        self.process_detection_enabled = enabled;
        self
    }

    pub fn with_process_parity(mut self, enabled: bool) -> Self {
        self.process_parity_enabled = enabled;
        self
    }

    pub fn with_community_crate_seeding(mut self, enabled: bool) -> Self {
        self.community_crate_seeding_enabled = enabled;
        self
    }
}

impl Default for RepoGraphBuildOptions {
    fn default() -> Self {
        Self::from_env()
    }
}

fn resolve_community_seed_by_crate(
    project_root: Option<&Path>,
    enabled: bool,
    crate_map: Option<&BTreeMap<PathBuf, String>>,
) -> Option<BTreeMap<PathBuf, String>> {
    if !enabled {
        return None;
    }
    let map = crate_map
        .cloned()
        .or_else(|| project_root.map(crate::canonical_graph::derive_crate_map))
        .filter(|map| !map.is_empty())?;
    Some(expand_crate_map_for_graph_paths(project_root, map))
}

fn expand_crate_map_for_graph_paths(
    project_root: Option<&Path>,
    mut crate_map: BTreeMap<PathBuf, String>,
) -> BTreeMap<PathBuf, String> {
    let Some(root) = project_root else {
        return crate_map;
    };
    let relative_entries: Vec<(PathBuf, String)> = crate_map
        .iter()
        .filter_map(|(path, name)| {
            path.strip_prefix(root)
                .ok()
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(|relative| (relative.to_path_buf(), name.clone()))
        })
        .collect();
    crate_map.extend(relative_entries);
    crate_map
}

/// Computed audit metadata for route-consumer edges. Kept out of persisted edge
/// structs so old graph artifacts remain compatible while op-layer callers can
/// still display the language chain that justified a `Fetches`/route link.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteEdgeLanguageChain {
    pub source_language: Option<String>,
    pub target_language: Option<String>,
    pub is_cross_language: bool,
}

impl RepoDependencyGraph {
    /// Read the inferred-route exclusion config attached to this graph.
    pub fn route_exclusion_config(&self) -> &RouteExclusionConfig {
        &self.route_exclusion_config
    }

    /// Return all precomputed layout positions keyed by stable node UID.
    pub fn layout_positions(&self) -> &BTreeMap<String, GraphLayoutPosition> {
        &self.layout_positions
    }

    /// Look up a precomputed layout coordinate by stable node UID.
    pub fn layout_position_by_uid(&self, stable_uid: &str) -> Option<GraphLayoutPosition> {
        self.layout_positions.get(stable_uid).copied()
    }

    /// Look up a precomputed layout coordinate by graph node identity.
    pub fn layout_position(&self, node: NodeIndex) -> Option<GraphLayoutPosition> {
        let uid = self.graph.node_weight(node)?.stable_uid();
        self.layout_position_by_uid(&uid)
    }

    /// Replace the inferred-route exclusion config sidecar.
    pub fn set_route_exclusion_config(&mut self, config: RouteExclusionConfig) {
        self.route_exclusion_config = config;
    }

    /// Replace the precomputed layout sidecar.
    pub fn set_layout_positions(
        &mut self,
        layout_positions: BTreeMap<String, GraphLayoutPosition>,
    ) {
        self.layout_positions = layout_positions;
    }

    /// Return all precomputed galaxy positions keyed by stable node UID.
    pub fn galaxy_positions(&self) -> &BTreeMap<String, GalaxyLayoutPosition> {
        &self.galaxy_positions
    }

    /// Look up a precomputed galaxy coordinate by graph node identity.
    pub fn galaxy_position(&self, node: NodeIndex) -> Option<GalaxyLayoutPosition> {
        let uid = self.graph.node_weight(node)?.stable_uid();
        self.galaxy_positions.get(&uid).copied()
    }

    /// Look up a precomputed collapsed-edge galaxy degree by node identity.
    pub fn galaxy_degree(&self, node: NodeIndex) -> Option<u32> {
        let uid = self.graph.node_weight(node)?.stable_uid();
        self.galaxy_degrees.get(&uid).copied()
    }

    /// Replace the precomputed galaxy layout sidecar (positions + degrees).
    pub fn set_galaxy_layout(&mut self, layout: crate::galaxy_layout::GalaxyLayout) {
        self.galaxy_positions = layout.positions;
        self.galaxy_degrees = layout.degrees;
    }

    /// Compute compat-safe language-chain audit metadata for a route edge.
    pub fn route_edge_language_chain(
        &self,
        source: NodeIndex,
        target: NodeIndex,
        kind: RepoGraphEdgeKind,
    ) -> Option<RouteEdgeLanguageChain> {
        if !matches!(
            kind,
            RepoGraphEdgeKind::HandlesRoute | RepoGraphEdgeKind::Fetches | RepoGraphEdgeKind::Route
        ) {
            return None;
        }
        let source_language = self.graph[source].language.clone();
        let target_language = self.graph[target].language.clone();
        let is_cross_language = source_language.is_some()
            && target_language.is_some()
            && source_language != target_language;
        Some(RouteEdgeLanguageChain {
            source_language,
            target_language,
            is_cross_language,
        })
    }

    pub fn is_cross_language_route_edge(
        &self,
        source: NodeIndex,
        target: NodeIndex,
        kind: RepoGraphEdgeKind,
    ) -> bool {
        self.route_edge_language_chain(source, target, kind)
            .is_some_and(|chain| chain.is_cross_language)
    }

    pub fn build(indices: &[ParsedScipIndex]) -> Self {
        Self::build_with_source(indices, None)
    }

    /// Build with explicit rollout toggles for tests and temporary extractor
    /// parity seams. Production callers should use [`Self::build`] /
    /// [`Self::build_with_source`] so env flags remain the single live control
    /// plane; this helper exists to construct the disabled baseline in parity
    /// tests without mutating process-wide environment variables.
    pub fn build_with_options(indices: &[ParsedScipIndex], options: RepoGraphBuildOptions) -> Self {
        Self::try_build_with_source_options(indices, None, options)
            .expect("repo graph build should satisfy enabled parity gates")
    }

    /// Build with an optional project-clone root. When `project_root` is
    /// `Some`, the edge-classification path will read source files via
    /// the [`crate::access_classifier::AccessClassifier`] to recover
    /// `Reads`/`Writes` edges for indexers (notably rust-analyzer) whose
    /// SCIP output doesn't carry `ReadAccess`/`WriteAccess` role bits.
    /// Tests that don't need access classification should call
    /// [`Self::build`] (no on-disk file required).
    pub fn build_with_source(indices: &[ParsedScipIndex], project_root: Option<&Path>) -> Self {
        Self::try_build_with_source_options(
            indices,
            project_root,
            RepoGraphBuildOptions::from_env(),
        )
        .expect("repo graph build should satisfy enabled parity gates")
    }

    /// Fallible form of [`Self::build_with_source`] used by canonical graph
    /// warming so temporary rollout parity failures surface as structured,
    /// actionable errors instead of panics.
    pub fn try_build_with_source(
        indices: &[ParsedScipIndex],
        project_root: Option<&Path>,
    ) -> Result<Self, String> {
        Self::try_build_with_source_options(
            indices,
            project_root,
            RepoGraphBuildOptions::from_env(),
        )
    }

    /// Fallible build with explicit temporary rollout toggles.
    pub fn try_build_with_source_options(
        indices: &[ParsedScipIndex],
        project_root: Option<&Path>,
        options: RepoGraphBuildOptions,
    ) -> Result<Self, String> {
        Self::try_build_with_source_options_and_crate_map(indices, project_root, options, None)
    }

    /// Fallible build with explicit temporary rollout toggles and an optional
    /// precomputed crate map for crate-aware community seeding. The map is used
    /// only when crate seeding is explicitly enabled in `options`.
    pub fn try_build_with_source_options_and_crate_map(
        indices: &[ParsedScipIndex],
        project_root: Option<&Path>,
        options: RepoGraphBuildOptions,
        crate_map: Option<&BTreeMap<PathBuf, String>>,
    ) -> Result<Self, String> {
        let community_seed_by_crate = resolve_community_seed_by_crate(
            project_root,
            options.community_crate_seeding_enabled,
            crate_map,
        );
        let mut builder = super::RepoDependencyGraphBuilder {
            project_root: project_root.map(|p| p.to_path_buf()),
            community_seed_by_crate,
            ..Default::default()
        };
        for index in indices {
            builder.add_index(index);
        }
        Self::finish_builder(builder, project_root, options)
    }

    /// Build the graph from a lazy iterator of `ScipFile` references
    /// without requiring the full [`ParsedScipIndex`] to be resident
    /// in memory. This is the bounded-memory entry point for the
    /// out-of-core pipeline.
    ///
    /// # Memory invariant
    ///
    /// Files are processed one-at-a-time from the iterator. Only one
    /// `ScipFile` is borrowed per iteration step — resident file data
    /// is **O(1)**, not `O(total_files)`.
    pub fn try_build_with_scip_files<'a, I>(
        files: I,
        workspace_slug: &str,
        external_symbols: &[ScipSymbol],
        project_root: Option<&Path>,
    ) -> Result<Self, String>
    where
        I: Iterator<Item = &'a ScipFile>,
    {
        Self::try_build_with_scip_files_options(
            files,
            workspace_slug,
            external_symbols,
            project_root,
            RepoGraphBuildOptions::from_env(),
        )
    }

    /// Like [`Self::try_build_with_scip_files`] with explicit build options.
    pub fn try_build_with_scip_files_options<'a, I>(
        files: I,
        workspace_slug: &str,
        external_symbols: &[ScipSymbol],
        project_root: Option<&Path>,
        options: RepoGraphBuildOptions,
    ) -> Result<Self, String>
    where
        I: Iterator<Item = &'a ScipFile>,
    {
        Self::try_build_with_scip_files_options_and_crate_map(
            files,
            workspace_slug,
            external_symbols,
            project_root,
            options,
            None,
        )
    }

    /// Like [`Self::try_build_with_scip_files_options`] with an optional
    /// precomputed crate map for crate-aware community seeding.
    pub fn try_build_with_scip_files_options_and_crate_map<'a, I>(
        files: I,
        workspace_slug: &str,
        external_symbols: &[ScipSymbol],
        project_root: Option<&Path>,
        options: RepoGraphBuildOptions,
        crate_map: Option<&BTreeMap<PathBuf, String>>,
    ) -> Result<Self, String>
    where
        I: Iterator<Item = &'a ScipFile>,
    {
        let community_seed_by_crate = resolve_community_seed_by_crate(
            project_root,
            options.community_crate_seeding_enabled,
            crate_map,
        );
        let mut builder = super::RepoDependencyGraphBuilder {
            project_root: project_root.map(|p| p.to_path_buf()),
            community_seed_by_crate,
            ..Default::default()
        };
        builder.add_scip_files(workspace_slug, external_symbols, files);
        Self::finish_builder(builder, project_root, options)
    }

    /// Build the graph from a **fallible** iterator of `ScipFile` entries.
    ///
    /// This variant is designed for the out-of-core pipeline where files
    /// are loaded from disk one-at-a-time and each load may fail
    /// (e.g. I/O error, deserialization error). Processing stops at the
    /// first error.
    ///
    /// # Memory invariant
    ///
    /// Only one `ScipFile` is resident per iteration step — resident
    /// file data is **O(1)**.
    pub fn try_build_with_scip_file_iter<I, F, E>(
        files: I,
        workspace_slug: &str,
        external_symbols: &[ScipSymbol],
        project_root: Option<&Path>,
    ) -> Result<Self, String>
    where
        I: Iterator<Item = Result<F, E>>,
        F: std::borrow::Borrow<ScipFile>,
        E: std::fmt::Display,
    {
        Self::try_build_with_scip_file_iter_options_and_crate_map(
            files,
            workspace_slug,
            external_symbols,
            project_root,
            RepoGraphBuildOptions::from_env(),
            None,
        )
    }

    /// Like [`Self::try_build_with_scip_file_iter`] with explicit build options
    /// and an optional precomputed crate map for crate-aware community seeding.
    pub fn try_build_with_scip_file_iter_options_and_crate_map<I, F, E>(
        files: I,
        workspace_slug: &str,
        external_symbols: &[ScipSymbol],
        project_root: Option<&Path>,
        options: RepoGraphBuildOptions,
        crate_map: Option<&BTreeMap<PathBuf, String>>,
    ) -> Result<Self, String>
    where
        I: Iterator<Item = Result<F, E>>,
        F: std::borrow::Borrow<ScipFile>,
        E: std::fmt::Display,
    {
        let community_seed_by_crate = resolve_community_seed_by_crate(
            project_root,
            options.community_crate_seeding_enabled,
            crate_map,
        );
        let mut builder = super::RepoDependencyGraphBuilder {
            project_root: project_root.map(|p| p.to_path_buf()),
            community_seed_by_crate,
            ..Default::default()
        };
        builder.add_scip_files_fallible(workspace_slug, external_symbols, files)?;
        Self::finish_builder(builder, project_root, options)
    }

    /// Common post-build pipeline: entry-point detection, process
    /// tracing, and complexity attachment. Shared by all build entry
    /// points (`try_build_with_source`, `try_build_with_scip_files`,
    /// `try_build_with_scip_file_iter`).
    fn finish_builder(
        builder: super::RepoDependencyGraphBuilder,
        project_root: Option<&Path>,
        options: RepoGraphBuildOptions,
    ) -> Result<Self, String> {
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
        if options.process_detection_enabled {
            // Temporary ykcg Process rollout seam: snapshot the graph after
            // entry-point detection but before Process enrichment. This is the
            // `DJINN_PROCESS_DETECTION=0` baseline shape; it must not become a
            // permanent alternate graph pipeline and should be deleted after
            // Process enrichment rollout.
            let parity_baseline = options.process_parity_enabled.then(|| graph.clone());
            let processes = crate::processes::detect_processes(&mut graph);
            graph.set_processes(processes);
            if let Some(baseline) = &parity_baseline {
                let parity_report =
                    crate::processes::assert_process_enrichment_graph_parity(baseline, &graph)
                        .map_err(|err| format!("process enrichment parity failed:\n{err}"))?;
                tracing::info!(
                    process_parity_report = %parity_report.render_for_ci(),
                    "repo graph build: process enrichment parity gate passed"
                );
            }
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
        Ok(graph)
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

    pub fn rank(&self) -> super::ranking::RepoGraphRanking {
        let page_rank_scores = super::ranking::compute_pagerank_sparse(
            &self.graph,
            super::constants::PAGE_RANK_DAMPING_FACTOR,
            super::constants::PAGE_RANK_ITERATIONS,
        );

        // PR F4: identify entry-point nodes (any node with an incoming
        // `EntryPointOf` edge) and BFS the graph from them via Outgoing
        // edges to compute `entry_point_distance`. Distance 0 sits on
        // the entry-point function itself; downstream callees grow
        // monotonically. Unreachable nodes stay `None`.
        let entry_distance = super::ranking::compute_entry_point_distance(&self.graph);

        let mut scored_nodes = Vec::with_capacity(self.graph.node_count());
        for node_index in self.graph.node_indices() {
            let node = &self.graph[node_index];
            // Route/Tool nodes are synthetic affordances. Keep them in the
            // PageRank projection so their edges still contribute to real
            // symbols, but do not expose them as ranked architecture hubs.
            if super::ranking::is_route_or_tool_node(node) {
                continue;
            }
            let page_rank = page_rank_scores[node_index.index()];
            let structural_weight = self.structural_weight(node_index);
            let score = page_rank * structural_weight;
            let is_entry_point = entry_distance
                .get(&node_index)
                .map(|d| *d == 0)
                .unwrap_or(false);
            scored_nodes.push(super::ranking::RankedRepoGraphNode {
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
        super::ranking::apply_rrf_fused_rank(&mut scored_nodes);

        scored_nodes.sort_by(|left, right| {
            right
                .fused_rank
                .total_cmp(&left.fused_rank)
                .then_with(|| right.page_rank.total_cmp(&left.page_rank))
                .then_with(|| right.structural_weight.total_cmp(&left.structural_weight))
                .then_with(|| left.key.cmp(&right.key))
        });

        super::ranking::RepoGraphRanking {
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

    pub fn is_singleton_route_without_consumers(&self, node_index: NodeIndex) -> bool {
        super::ranking::is_singleton_route_without_consumers(&self.graph, node_index)
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
                if kind_filter.is_none() && is_default_hidden_synthetic_kind(node.kind) {
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
        let weight = super::edge::edge_weight_for(RepoGraphEdgeKind::StepInProcess);
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

    /// PR s6ch / ykcg: stamp a `HandlesRoute` edge from a synthetic
    /// [`RepoGraphNodeKind::Route`] node to the handler
    /// [`RepoGraphNodeKind::Symbol`] node. Route extraction is out of
    /// scope for this model task; callers supply the detector `reason`
    /// and may override confidence, which is clamped to `[0, 1]`.
    #[allow(dead_code)] // Route detectors land in a follow-up task.
    pub(crate) fn add_handles_route_edge(
        &mut self,
        route: NodeIndex,
        handler: NodeIndex,
        reason: &str,
        confidence: Option<f64>,
    ) {
        self.add_route_metadata_edge(
            route,
            handler,
            RepoGraphEdgeKind::HandlesRoute,
            reason,
            confidence,
        );
    }

    /// PR s6ch / ykcg: stamp a `Fetches` edge from a caller/client
    /// [`RepoGraphNodeKind::Symbol`] to the synthetic
    /// [`RepoGraphNodeKind::Route`] it invokes. Consumer inference is a
    /// side channel; callers supply an explanatory `reason` and may
    /// override confidence, which is clamped to `[0, 1]`.
    #[allow(dead_code)] // Client route inference lands in a follow-up task.
    pub(crate) fn add_fetches_edge(
        &mut self,
        caller: NodeIndex,
        route: NodeIndex,
        reason: &str,
        confidence: Option<f64>,
    ) {
        self.add_route_metadata_edge(
            caller,
            route,
            RepoGraphEdgeKind::Fetches,
            reason,
            confidence,
        );
    }

    fn add_route_metadata_edge(
        &mut self,
        source: NodeIndex,
        target: NodeIndex,
        kind: RepoGraphEdgeKind,
        reason: &str,
        confidence: Option<f64>,
    ) {
        debug_assert!(matches!(
            kind,
            RepoGraphEdgeKind::HandlesRoute | RepoGraphEdgeKind::Fetches
        ));
        self.graph.add_edge(
            source,
            target,
            RepoGraphEdge {
                kind,
                weight: edge_weight(kind),
                evidence_count: 1,
                confidence: confidence
                    .unwrap_or_else(|| edge_confidence_floor(kind))
                    .clamp(0.0, 1.0),
                reason: Some(reason.to_string()),
                step: None,
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
            // PR s6ch / cs4v: route metadata is not applicable to
            // process nodes — the field set is shared across kinds
            // so the struct needs the slot but it stays `None`.
            route_framework: None,
            route_handler_symbol: None,
        };
        let idx = self.graph.add_node(node);
        self.node_lookup.insert(key, idx);
        add_name_index_entry(&mut self.name_index, &self.graph[idx].display_name, idx);
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
            // PR s6ch / cs4v: route metadata is not applicable to
            // table nodes — the field set is shared across kinds so
            // the struct needs the slot but it stays `None`.
            route_framework: None,
            route_handler_symbol: None,
        };
        let idx = self.graph.add_node(node);
        self.node_lookup.insert(key, idx);
        add_name_index_entry(&mut self.name_index, &self.graph[idx].display_name, idx);
        idx
    }

    /// PR s6ch / cs4v: register a synthetic [`RepoGraphNodeKind::Route`]
    /// node and return its [`NodeIndex`]. Idempotent on `id`. The
    /// `id` is the stable route id shaped as `"<METHOD> <path>
    /// (<framework>)"`, e.g. `"GET /api/agents (axum)"`. `framework`
    /// and `handler_symbol` are stored on the node's `route_framework`
    /// / `route_handler_symbol` metadata fields so callers can recover
    /// the handler's [`RepoNodeKey::Symbol`] back-reference without
    /// walking the graph. The handler-symbol edge itself is stamped
    /// separately by the route extractor (out of scope for cs4v).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn ensure_route_node(
        &mut self,
        id: &str,
        display_name: &str,
        language: Option<&str>,
        workspace: Option<&str>,
        source_file: Option<&Path>,
        framework: Option<&str>,
        handler_symbol: Option<&str>,
    ) -> NodeIndex {
        let route_id = synthetic_source_scoped_key(
            "route",
            id,
            source_file,
            None,
            handler_symbol,
            self.graph.node_count(),
        );
        let key = RepoNodeKey::Route(route_id);
        if let Some(&idx) = self.node_lookup.get(&key) {
            return idx;
        }
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::Route,
            display_name: display_name.to_string(),
            language: language.map(str::to_string),
            file_path: source_file.map(Path::to_path_buf),
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: workspace.map(str::to_string),
            route_framework: framework.map(str::to_string),
            route_handler_symbol: handler_symbol.map(str::to_string),
        };
        let idx = self.graph.add_node(node);
        self.node_lookup.insert(key, idx);
        add_name_index_entry(&mut self.name_index, &self.graph[idx].display_name, idx);
        idx
    }

    /// PR s6ch / cs4v: register a synthetic [`RepoGraphNodeKind::Tool`]
    /// node and return its [`NodeIndex`]. Idempotent on `id`. The
    /// `id` is the stable tool id (e.g. `"agents.list"`). No
    /// extractor lands in cs4v — the helper exists so the graph can
    /// carry tool nodes when proposal ykcg #6 ships.
    #[allow(dead_code)] // cs4v lands the helper ahead of its first caller.
    pub(crate) fn ensure_tool_node(
        &mut self,
        id: &str,
        display_name: &str,
        language: Option<&str>,
        workspace: Option<&str>,
        source_file: Option<&Path>,
    ) -> NodeIndex {
        let tool_id = synthetic_source_scoped_key(
            "tool",
            id,
            source_file,
            workspace,
            None,
            self.graph.node_count(),
        );
        let key = RepoNodeKey::Tool(tool_id);
        if let Some(&idx) = self.node_lookup.get(&key) {
            return idx;
        }
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::Tool,
            display_name: display_name.to_string(),
            language: language.map(str::to_string),
            file_path: source_file.map(Path::to_path_buf),
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: workspace.map(str::to_string),
            // Tool nodes are forward-compatible surface only — no
            // framework / handler back-reference fields exist on the
            // shared struct for them.
            route_framework: None,
            route_handler_symbol: None,
        };
        let idx = self.graph.add_node(node);
        self.node_lookup.insert(key, idx);
        add_name_index_entry(&mut self.name_index, &self.graph[idx].display_name, idx);
        idx
    }

    /// Stamp a route extraction edge with explicit confidence/reason.
    pub(crate) fn add_route_edge(
        &mut self,
        source: NodeIndex,
        target: NodeIndex,
        kind: RepoGraphEdgeKind,
        confidence: f64,
        reason: &str,
    ) {
        debug_assert!(matches!(
            kind,
            RepoGraphEdgeKind::HandlesRoute | RepoGraphEdgeKind::Fetches
        ));
        self.graph.add_edge(
            source,
            target,
            RepoGraphEdge {
                kind,
                weight: edge_weight(kind),
                evidence_count: 1,
                confidence,
                reason: Some(reason.to_string()),
                step: None,
            },
        );
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

    /// Register a placeholder symbol for a route handler that appeared in
    /// source but could not be matched to a SCIP definition.
    pub(crate) fn ensure_unresolved_route_handler_node(
        &mut self,
        file_path: &Path,
        handler: &str,
    ) -> NodeIndex {
        let symbol = format!("axum-unresolved-handler:{}:{handler}", file_path.display());
        let key = RepoNodeKey::Symbol(symbol.clone());
        if let Some(&idx) = self.node_lookup.get(&key) {
            return idx;
        }
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::Symbol,
            display_name: handler.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(file_path.to_path_buf()),
            symbol: Some(symbol),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: None,
            route_framework: None,
            route_handler_symbol: None,
        };
        let idx = self.graph.add_node(node);
        self.node_lookup.insert(key, idx);
        idx
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

impl RepoDependencyGraph {
    /// Replace the community sidecar with a fresh detection pass
    /// result. Rebuilds the reverse `community_lookup` index.
    pub(super) fn install_communities(&mut self, communities: Vec<crate::communities::Community>) {
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

pub(super) fn build_name_index(
    graph: &DiGraph<RepoGraphNode, RepoGraphEdge>,
) -> BTreeMap<String, Vec<NodeIndex>> {
    let mut index: BTreeMap<String, Vec<NodeIndex>> = BTreeMap::new();
    for node_index in graph.node_indices() {
        let node = &graph[node_index];
        add_name_index_entry(&mut index, &node.display_name, node_index);
    }
    index
}

fn add_name_index_entry(
    index: &mut BTreeMap<String, Vec<NodeIndex>>,
    display_name: &str,
    node_index: NodeIndex,
) {
    let key = display_name.to_lowercase();
    let indices = index.entry(key).or_default();
    if !indices.contains(&node_index) {
        indices.push(node_index);
    }
}

pub(crate) fn is_default_hidden_synthetic_kind(kind: RepoGraphNodeKind) -> bool {
    matches!(kind, RepoGraphNodeKind::Tool)
}

/// PR F2: build the reverse `node_index → process positions` lookup
/// from a freshly-set process list. The same node can appear in
/// multiple processes (a shared utility called by several entry
/// points), so the value is `Vec<usize>` rather than `Option<usize>`.
pub(super) fn build_process_lookup(
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

/// Function-likeness for complexity attachment, tolerant of indexers that
/// don't populate `SymbolInformation.kind`. scip-typescript emits
/// `UnspecifiedKind(0)` for every symbol, so relying on the kind alone
/// silently disabled complexity for ALL TypeScript code. Fall back to the
/// SCIP descriptor grammar: a method/function descriptor is
/// `name '(' disambiguator? ')' '.'`, so a trailing `")."` marks the
/// symbol as function-like even without a kind.
fn is_function_like_node(node: &RepoGraphNode) -> bool {
    if is_function_like_symbol_kind(node.symbol_kind.as_ref()) {
        return true;
    }
    let kind_unknown = matches!(node.symbol_kind, None | Some(ScipSymbolKind::Unknown(_)));
    kind_unknown
        && node
            .symbol
            .as_deref()
            .is_some_and(|symbol| symbol.ends_with(")."))
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
                if !is_function_like_node(node) {
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

/// Returns `true` when `node` is "owned by" one of the changed files:
/// - file nodes whose path is in the set
/// - symbol nodes whose `file_path` is in the set *and* that are not external
#[cfg(test)]
pub(super) fn is_owned_by_changed_file(
    node: &RepoGraphNode,
    changed_files: &BTreeSet<PathBuf>,
) -> bool {
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
        // PR s6ch / cs4v: synthetic route / tool nodes are never
        // owned by a single source file — the route id is shaped
        // `METHOD path (framework)` and the handler back-reference
        // is denormalized, so a file-level changed-file strip
        // shouldn't drop them. The next warm re-runs the route
        // extractor from scratch, mirroring the `Process` /
        // `Table` policy above.
        RepoGraphNodeKind::Route => false,
        RepoGraphNodeKind::Tool => false,
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
pub(super) fn derive_edge_confidence(
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
