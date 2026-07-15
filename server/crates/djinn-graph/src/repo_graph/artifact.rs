//! [`RepoGraphArtifact`] and the bincode/JSON (de)serialization paths.
//!
//! The artifact is the persisted form of the in-memory [`RepoDependencyGraph`]
//! (see `super::RepoDependencyGraph`). The bincode/JSON (de)serialization
//! surface lives here so the rest of the crate can stay focused on building,
//! querying, and ranking the graph.
//!
//! V10 compatibility: the artifact version deliberately remains `10` across the
//! additive `workspace` field on [`super::RepoGraphNode`] (PR
//! `additive-repographnode-workspace-field-with-v10-artifact-compatibility`).
//! Pre-workspace v10 blobs are accepted via the
//! [`deserialize_repo_graph_artifact_bincode`] seam rather than raw
//! `bincode::deserialize`. Do not bump the version or change the field order
//! of any of the structs in this file without also re-reading that case note.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::complexity::ComplexityMetrics;
use crate::galaxy_layout::GalaxyLayoutPosition;
use crate::layout::GraphLayoutPosition;
use crate::scip_parser::{ScipSymbolKind, ScipVisibility};

use super::edge::RepoGraphEdgeKind;
use super::edge::{EdgeConfidenceTier, edge_confidence_tier};
use super::node::{RepoGraphNode, RepoGraphNodeKind, RepoNodeKey};

/// Public wire shape for first-class HTTP route metadata.
///
/// Downstream graph-ops serialize this compact reference instead of
/// re-deriving route fields from [`RepoGraphNode`] identity/display strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRef {
    pub id: String,
    pub method: String,
    pub path: String,
    pub framework: String,
    pub handler_label: Option<String>,
}

/// Durable sidecar controlling which inferred route/consumer edges should be
/// suppressed or downgraded by graph operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteExclusionConfig {
    pub health_path_globs: Vec<String>,
    pub param_only_paths: bool,
    pub min_confidence_for_consumer_edge: f64,
    pub excluded_frameworks: Vec<String>,
}

impl Default for RouteExclusionConfig {
    fn default() -> Self {
        Self {
            health_path_globs: [
                "/health", "/healthz", "/ping", "/readyz", "/livez", "/metrics",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            param_only_paths: true,
            min_confidence_for_consumer_edge: 0.5,
            excluded_frameworks: Vec::new(),
        }
    }
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
    /// Config sidecar for inferred route/consumer-edge exclusions.
    #[serde(default)]
    pub route_exclusion_config: RouteExclusionConfig,
    /// Deterministic warm-time layout positions keyed by stable node UID.
    #[serde(default)]
    pub layout_positions: BTreeMap<String, GraphLayoutPosition>,
    /// Proposal lmkv: deterministic warm-time 3D "galaxy" layout positions
    /// keyed by stable node UID. Additive trailing field — bincode is
    /// positional, so blobs written before this field fall back through
    /// [`RepoGraphArtifactV10WithoutGalaxyLayout`] and hydrate this as empty.
    /// A `code_graph snapshot` then omits the per-node galaxy coordinates and
    /// the UI transparently falls back to its worker layout.
    #[serde(default)]
    pub galaxy_positions: BTreeMap<String, GalaxyLayoutPosition>,
    /// Proposal lmkv: per-node degree from the *collapsed* galaxy edge view
    /// (external symbols dropped, usage edges folded to one file→file edge per
    /// pair), keyed by stable node UID. Shipped so the UI reuses the server
    /// degree instead of recomputing it. Additive trailing field; see
    /// [`Self::galaxy_positions`].
    #[serde(default)]
    pub galaxy_degrees: BTreeMap<String, u32>,
}

#[derive(Debug, Deserialize)]
struct RepoGraphArtifactV10WithoutGalaxyLayout {
    version: u32,
    nodes: Vec<RepoGraphNode>,
    edges: Vec<RepoGraphArtifactEdge>,
    #[serde(default)]
    symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
    #[serde(default)]
    communities: Vec<crate::communities::Community>,
    #[serde(default)]
    processes: Vec<RepoGraphArtifactProcess>,
    #[serde(default)]
    route_exclusion_config: RouteExclusionConfig,
    #[serde(default)]
    layout_positions: BTreeMap<String, GraphLayoutPosition>,
}

#[derive(Debug, Deserialize)]
struct RepoGraphArtifactV10WithoutLayoutPositions {
    version: u32,
    nodes: Vec<RepoGraphNode>,
    edges: Vec<RepoGraphArtifactEdge>,
    #[serde(default)]
    symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
    #[serde(default)]
    communities: Vec<crate::communities::Community>,
    #[serde(default)]
    processes: Vec<RepoGraphArtifactProcess>,
    #[serde(default)]
    route_exclusion_config: RouteExclusionConfig,
}

#[derive(Debug, Deserialize)]
struct RepoGraphArtifactV10WithoutRouteExclusionConfig {
    version: u32,
    nodes: Vec<RepoGraphNode>,
    edges: Vec<RepoGraphArtifactEdge>,
    #[serde(default)]
    symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
    #[serde(default)]
    communities: Vec<crate::communities::Community>,
    #[serde(default)]
    processes: Vec<RepoGraphArtifactProcess>,
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
struct RepoGraphArtifactV10WithoutRouteMetadata {
    version: u32,
    nodes: Vec<RepoGraphNodeV10WithoutRouteMetadata>,
    edges: Vec<RepoGraphArtifactEdge>,
    #[serde(default)]
    symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
    #[serde(default)]
    communities: Vec<crate::communities::Community>,
    #[serde(default)]
    processes: Vec<RepoGraphArtifactProcess>,
}

#[derive(Debug, Deserialize)]
struct RepoGraphNodeV10WithoutRouteMetadata {
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
    #[serde(default)]
    workspace: Option<String>,
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

impl From<RepoGraphArtifactV10WithoutGalaxyLayout> for RepoGraphArtifact {
    fn from(old: RepoGraphArtifactV10WithoutGalaxyLayout) -> Self {
        Self {
            version: old.version,
            nodes: old.nodes,
            edges: old.edges,
            symbol_ranges: old.symbol_ranges,
            communities: old.communities,
            processes: old.processes,
            route_exclusion_config: old.route_exclusion_config,
            layout_positions: old.layout_positions,
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        }
    }
}

impl From<RepoGraphArtifactV10WithoutLayoutPositions> for RepoGraphArtifact {
    fn from(old: RepoGraphArtifactV10WithoutLayoutPositions) -> Self {
        Self {
            version: old.version,
            nodes: old.nodes,
            edges: old.edges,
            symbol_ranges: old.symbol_ranges,
            communities: old.communities,
            processes: old.processes,
            route_exclusion_config: old.route_exclusion_config,
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        }
    }
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
            route_exclusion_config: RouteExclusionConfig::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        }
    }
}

impl From<RepoGraphArtifactV10WithoutRouteExclusionConfig> for RepoGraphArtifact {
    fn from(old: RepoGraphArtifactV10WithoutRouteExclusionConfig) -> Self {
        Self {
            version: old.version,
            nodes: old.nodes,
            edges: old.edges,
            symbol_ranges: old.symbol_ranges,
            communities: old.communities,
            processes: old.processes,
            route_exclusion_config: RouteExclusionConfig::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        }
    }
}

impl From<RepoGraphArtifactV10WithoutRouteMetadata> for RepoGraphArtifact {
    fn from(old: RepoGraphArtifactV10WithoutRouteMetadata) -> Self {
        Self {
            version: old.version,
            nodes: old.nodes.into_iter().map(RepoGraphNode::from).collect(),
            edges: old.edges,
            symbol_ranges: old.symbol_ranges,
            communities: old.communities,
            processes: old.processes,
            route_exclusion_config: RouteExclusionConfig::default(),
            layout_positions: BTreeMap::new(),
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        }
    }
}

impl From<RepoGraphNodeV10WithoutRouteMetadata> for RepoGraphNode {
    fn from(old: RepoGraphNodeV10WithoutRouteMetadata) -> Self {
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
            workspace: old.workspace,
            // PR s6ch / cs4v: route metadata was appended after
            // `workspace` inside artifact v10. Bincode is positional, so
            // defaulted serde fields are not enough for already-persisted
            // blobs that end at `workspace`; this compatibility shape
            // preserves v10 loading when no Route/Tool metadata fields are
            // present on disk.
            route_framework: None,
            route_handler_symbol: None,
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
            // PR s6ch / cs4v: route metadata is additive and not
            // present in any v10 artifact. Older blobs hydrate as
            // `None` so the v10 compat path stays open.
            route_framework: None,
            route_handler_symbol: None,
        }
    }
}

/// Deserialize a repo-graph bincode artifact, accepting the current v10 node
/// layout, v10 blobs that include `workspace` but predate route metadata, and
/// pre-workspace v10 blobs. The artifact version deliberately remains v10, so
/// callers on the persisted cache path must use this compatibility seam instead
/// of raw `bincode::deserialize`.
pub fn deserialize_repo_graph_artifact_bincode(blob: &[u8]) -> Result<RepoGraphArtifact, String> {
    match bincode::deserialize::<RepoGraphArtifact>(blob) {
        Ok(artifact) => Ok(artifact),
        Err(current_err) => {
            // Proposal lmkv: blobs written before the galaxy sidecar end at
            // `layout_positions`. Try that shape first so already-persisted
            // v10 blobs still load (galaxy fields hydrate empty).
            if let Ok(artifact) =
                bincode::deserialize::<RepoGraphArtifactV10WithoutGalaxyLayout>(blob)
            {
                return Ok(RepoGraphArtifact::from(artifact));
            }
            match bincode::deserialize::<RepoGraphArtifactV10WithoutLayoutPositions>(blob) {
                Ok(artifact) => Ok(RepoGraphArtifact::from(artifact)),
                Err(layout_compat_err) => {
                    match bincode::deserialize::<RepoGraphArtifactV10WithoutRouteExclusionConfig>(
                        blob,
                    ) {
                        Ok(artifact) => Ok(RepoGraphArtifact::from(artifact)),
                        Err(config_compat_err) => {
                            match bincode::deserialize::<RepoGraphArtifactV10WithoutRouteMetadata>(
                                blob,
                            ) {
                                Ok(artifact) => Ok(RepoGraphArtifact::from(artifact)),
                                Err(route_compat_err) => {
                                    bincode::deserialize::<RepoGraphArtifactV10WithoutWorkspace>(
                                        blob,
                                    )
                                    .map(RepoGraphArtifact::from)
                                    .map_err(|workspace_compat_err| {
                                        format!(
                                            "deserialize graph: {current_err}; v10 pre-layout fallback also failed: {layout_compat_err}; v10 pre-route-exclusion-config fallback also failed: {config_compat_err}; v10 pre-route-metadata fallback also failed: {route_compat_err}; v10 pre-workspace fallback also failed: {workspace_compat_err}"
                                        )
                                    })
                                }
                            }
                        }
                    }
                }
            }
        }
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

impl RepoGraphArtifactEdge {
    pub fn confidence_tier(&self) -> EdgeConfidenceTier {
        edge_confidence_tier(self.kind, self.confidence, self.reason.as_deref())
    }
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
