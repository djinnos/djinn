//! `djinn-graph::repo_graph` — in-memory repository dependency graph
//! data structure plus its builders, queries, serialization, and
//! persistence helpers.
//!
//! The module used to be a single 4,573-line `repo_graph.rs` file. The
//! follow-up tasks in this wave split it into focused submodules so
//! individual concerns can be reviewed and tested in isolation:
//!
//! | submodule  | concern                                                     |
//! |------------|-------------------------------------------------------------|
//! | `constants`| edge confidence/weight tables, PageRank knobs, version stamp |
//! | `node`     | `RepoGraphNode`, `RepoNodeKey`, `RepoGraphNodeKind`           |
//! | `edge`     | `RepoGraphEdge`, `RepoGraphEdgeKind`, edge weight/confidence |
//! | `tests`    | the `repo_graph::tests` test module                          |
//! | `artifact` | `RepoGraphArtifact` + v10 compat (sibling task `yxp7`)       |
//! | `builder`  | `RepoDependencyGraphBuilder` (this task, `3hrr`)             |
//! | `graph`    | `RepoDependencyGraph` + queries (sibling task `our5`)        |
//! | `ranking`  | PageRank / RRF (sibling task `our5`)                         |
//!
//! All public types are re-exported here so downstream consumers
//! (`crate::repo_graph::RepoGraphNode`, etc.) keep working without
//! edits.

mod artifact;
mod builder;
mod constants;
mod crate_aggregation;
mod edge;
mod graph;
mod node;
mod ranking;

#[cfg(test)]
mod tests;

pub use self::crate_aggregation::build_crate_graph;

// Re-exports for the public API — see `crates/djinn-control-plane/src/
// tools/graph_tools.rs`, `server/src/mcp_bridge.rs`, `cluster_doc.rs`,
// `communities.rs`, etc. for the consumer side.
pub use self::artifact::{
    RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphArtifactProcess,
    RepoGraphArtifactSymbolRange, RouteExclusionConfig, RouteRef,
    deserialize_repo_graph_artifact_bincode,
};
pub use self::constants::{
    REASON_TRAIT_DISPATCH_CALL, REASON_TRAIT_DISPATCH_FANOUT, REASON_TRAIT_DISPATCH_SUPPRESSED,
    TRAIT_DISPATCH_FANOUT_CAP,
};
pub use self::constants::{REPO_GRAPH_ARTIFACT_VERSION, is_test_path};
pub use self::edge::{
    EdgeConfidenceTier, RepoGraphEdge, RepoGraphEdgeKind, edge_confidence_floor,
    edge_confidence_tier, promote_fetches_confidence_with_import_evidence,
};
pub use self::graph::{
    RepoDependencyGraph, RepoGraphBuildOptions, RouteEdgeLanguageChain, SymbolRange,
};
pub use self::node::{
    RepoGraphNode, RepoGraphNodeKind, RepoGraphSearchHit, RepoNodeKey, is_route_or_tool_node,
    stable_node_uid,
};
pub use self::ranking::{
    RankedRepoGraphNode, RepoGraphRanking, is_singleton_route_without_consumers,
};

/// `RepoDependencyGraphBuilder` lives in [`self::builder`] (see
/// `repo_graph/builder.rs`). The `impl` block in `mod.rs` (`build_with_source`
/// and `patch_changed_files`) and the `repo_graph::tests` test module both
/// reference it through this re-export so the submodule itself can stay
/// private (`mod builder;` above).
pub(crate) use self::builder::RepoDependencyGraphBuilder;

// Re-exports for sibling submodule bodies (the impl blocks in
// `mod.rs` itself) AND the `repo_graph::tests` test module. `pub(crate)`
// so the items stay crate-internal while still being reachable from
// `mod.rs` and its descendants (including the test module, which is a
// child of `mod.rs`).
//
// `#[allow(unused_imports)]` because the `EDGE_CONFIDENCE_*` constants
// and ranking helpers are only consumed by the test module / sibling
// submodules — the lib build doesn't directly reference them. Without
// the attribute the lib build would emit unused-imports warnings.
#[allow(unused_imports)]
pub(crate) use self::constants::{
    EDGE_CONFIDENCE_LOCAL_PENALTY, EDGE_CONFIDENCE_READS, EDGE_CONFIDENCE_WRITES,
    PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS,
};
pub(crate) use self::edge::{edge_weight, edge_weight_for};
pub(crate) use self::graph::is_default_hidden_synthetic_kind;
#[allow(unused_imports)]
pub(crate) use self::ranking::{
    apply_rrf_fused_rank, compute_entry_point_distance, compute_pagerank_sparse,
};

// `BTreeSet` and `ParsedScipIndex` are only consumed by the test module
// (in `patch_changed_files`'s `#[cfg(test)]` body and the test fixture
// builders). Marked with `#[allow(unused_imports)]` so the lib build
// (without `--tests`) does not emit unused-imports warnings.
#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[allow(unused_imports)]
use crate::scip_parser::ParsedScipIndex;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

#[derive(Debug, Clone, PartialEq)]
pub struct CrateNode {
    pub name: String,
    pub manifest_path: PathBuf,
    pub loc: usize,
    pub node_count: usize,
    pub fan_in: f64,
    pub fan_out: f64,
    pub inbound_weight: f64,
    pub outbound_weight: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CrateEdge {
    pub source: String,
    pub target: String,
    pub weight: f64,
    pub edge_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CrateGraph {
    pub crates: Vec<CrateNode>,
    pub edges: Vec<CrateEdge>,
}

#[cfg(test)]
use self::graph::is_owned_by_changed_file;
use self::graph::{build_name_index, build_process_lookup, derive_edge_confidence};

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
            route_exclusion_config: self.route_exclusion_config.clone(),
            layout_positions: self.layout_positions.clone(),
            galaxy_positions: self.galaxy_positions.clone(),
            galaxy_degrees: self.galaxy_degrees.clone(),
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
            // PR s6ch / 92z7: rehydrate the route exclusion config
            // from the artifact so callers see the same
            // health-path / param-only / below-confidence-floor
            // exclusions the warmer persisted. Defaults to the
            // baseline config (health globs, param_only_paths=true,
            // min_confidence_for_consumer_edge=0.5) for legacy
            // artifacts that pre-date the sidecar — see
            // `RepoGraphArtifactV10WithoutRouteExclusionConfig` in
            // `artifact.rs` for the deserialization compat path.
            route_exclusion_config: artifact.route_exclusion_config.clone(),
            layout_positions: if artifact.layout_positions.is_empty() && !artifact.nodes.is_empty()
            {
                BTreeMap::new()
            } else {
                artifact.layout_positions.clone()
            },
            galaxy_positions: artifact.galaxy_positions.clone(),
            galaxy_degrees: artifact.galaxy_degrees.clone(),
        };
        // PR F3: rehydrate the community sidecar verbatim — node
        // positions in the artifact match `NodeIndex` 0..n thanks to the
        // ordered `add_node` loop above.
        if !artifact.communities.is_empty() {
            out.install_communities(artifact.communities.clone());
        }
        if out.layout_positions.is_empty() && out.node_count() > 0 {
            out.layout_positions = crate::layout::derive_layout_positions(&out);
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
            route_exclusion_config: artifact.route_exclusion_config,
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
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
