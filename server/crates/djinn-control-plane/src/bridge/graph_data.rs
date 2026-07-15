// djinn:allow-oversize
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct SemanticQueryEmbedding {
    pub values: Vec<f32>,
}

// ── Repo Graph ──────────────────────────────────────────────────────────────────
// Bridge for RepoDependencyGraph queries. The server implements this by
// building the graph from SCIP artifacts; djinn-control-plane/djinn-agent never depend
// on petgraph or SCIP protobuf types directly.

/// A neighbor of a node in the repository dependency graph.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphNeighbor {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub kind: String,
    pub display_name: String,
    pub edge_kind: String,
    pub edge_weight: f64,
    pub direction: String,
}

/// A ranked node from PageRank + structural weight scoring.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RankedNode {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub kind: String,
    pub display_name: String,
    pub score: f64,
    pub page_rank: f64,
    pub structural_weight: f64,
    pub inbound_edge_weight: f64,
    pub outbound_edge_weight: f64,
    // v8: added with parse-time scoped-variable filter; see version bump in
    // sibling change. PR F4: entry-point + bucketing side-channels exposed
    // by the new multi-signal RRF ranker so the UI can group results by
    // process / community and surface entry-point status without a second
    // round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
    #[serde(default)]
    pub is_entry_point: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_point_distance: Option<u32>,
}

/// A search hit from the name-index lookup or hybrid RRF fusion. Returned
/// by `search`. PR B4 added `match_kind` (which signal contributed the
/// hit — `"name"` / `"lexical"` / `"semantic"` / `"structural"` / `"hybrid"`)
/// for debug / Pulse-panel surfaces; old clients that don't read it stay
/// unaffected because the field is `skip_serializing_if = "Option::is_none"`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchHit {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub kind: String,
    pub display_name: String,
    pub score: f64,
    pub file: Option<String>,
    /// PR B4: tags the signal that surfaced this hit (or `"hybrid"` when
    /// it was promoted by RRF fusion across multiple signals). `None` for
    /// the legacy `mode=name` fast path so the schema stays additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_kind: Option<String>,
}

/// A member of a strongly-connected component.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CycleMember {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub display_name: String,
    pub kind: String,
}

/// A strongly-connected component returned by `cycles`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CycleGroup {
    pub size: usize,
    pub members: Vec<CycleMember>,
}

/// An orphan node (zero incoming references) returned by `orphans`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OrphanEntry {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub kind: String,
    pub display_name: String,
    pub file: Option<String>,
    pub visibility: String,
}

/// A single hop in a `path` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PathHop {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub edge_kind: String,
}

/// Result of a `path` query — the shortest dependency path from one node to
/// another.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PathResult {
    pub from: String,
    pub to: String,
    pub hops: Vec<PathHop>,
    pub length: usize,
}

/// An edge enumerated by `edges`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EdgeEntry {
    pub from: String,
    pub to: String,
    pub edge_kind: String,
    pub edge_weight: f64,
    pub confidence: f64,
    pub confidence_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// PR s6ch / 92z7: machine-readable explanation when the project
    /// route-exclusion policy downgraded the edge to a suggestion
    /// (`"below-confidence-floor"`, `"health-path"`, `"param-only-path"`).
    /// `None` for edges the active policy treats as a hard dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
}

/// A single `(file, start_line, end_line)` hunk from a parsed diff. The
/// caller supplies one of these per `git diff --unified=0` hunk when
/// invoking the `diff_touches` op on the `code_graph` tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ChangedRange {
    /// Repository-relative path of the file the hunk lives in.
    pub file: String,
    /// Inclusive 1-indexed first line of the hunk.
    pub start_line: i64,
    /// Inclusive 1-indexed last line of the hunk. Defaults to `start_line`
    /// when the caller passed a single-line hunk.
    pub end_line: Option<i64>,
}

/// A single symbol (or file) whose definition range encloses a queried
/// line span. Emitted by the `symbols_at` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolAtHit {
    /// Canonical node key — SCIP symbol string for symbol hits, file path
    /// (file: prefix) for file hits.
    pub key: String,
    #[serde(default)]
    pub uid: String,
    /// Either `"file"` or `"symbol"`.
    pub kind: String,
    pub display_name: String,
    pub file: Option<String>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub visibility: Option<String>,
    pub symbol_kind: Option<String>,
}

/// Result of a `diff_touches` query — the set of base-graph symbols whose
/// definition ranges overlap any of the caller's diff hunks, plus the
/// affected-file and unknown-file rollups.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffTouchesResult {
    pub touched_symbols: Vec<TouchedSymbol>,
    /// Files from the caller's `changed_ranges` that resolved to at least
    /// one base-graph file node (deduplicated, preserves input order).
    pub affected_files: Vec<String>,
    /// Files from the caller's `changed_ranges` that have no matching
    /// file node in the base graph — i.e. pure additions, untracked
    /// files, or paths that fall outside SCIP coverage.
    pub unknown_files: Vec<String>,
}

/// A single touched symbol surfaced by the `diff_touches` op, enriched
/// with fan-in/fan-out counts so callers can triage blast radius without
/// issuing a follow-up `neighbors` query.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TouchedSymbol {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub display_name: String,
    pub kind: String,
    pub symbol_kind: Option<String>,
    pub visibility: Option<String>,
    pub file: Option<String>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    /// Incoming edge count in the base graph.
    pub fan_in: usize,
    /// Outgoing edge count in the base graph.
    pub fan_out: usize,
}

/// PageRank tier bucket for a touched symbol. Computed at request time
/// against the current project graph (not the from/to shas), so review
/// weight reflects "what matters now" rather than a stale snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PagerankTier {
    High,
    Medium,
    Low,
}

/// Whether a symbol was added (post-image only), modified (overlapping
/// pre and post), or deleted (no symbol left at this range in head).
///
/// PR C4 detects `Modified` for any symbol whose enclosing range
/// overlaps a head-side hunk. `Added` and `Deleted` are reserved
/// values — full add/delete classification requires a second graph
/// build at the from-sha; left as an enum stub so the wire shape is
/// stable for future enhancements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// A single symbol surfaced by the `detect_changes` op. Distinct from
/// [`TouchedSymbol`] (the `diff_touches` payload, which carries
/// fan-in/fan-out) because review weight is driven by PageRank tier
/// here, not raw degree.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DetectedTouchedSymbol {
    /// SCIP symbol string (canonical node uid).
    pub uid: String,
    /// Human-friendly display name.
    pub name: String,
    /// SCIP symbol kind (e.g. `"function"`, `"method"`) lowercased,
    /// or `"file"` when the touched node is a file rather than a symbol.
    pub kind: String,
    /// Repository-relative file the symbol lives in.
    pub file_path: String,
    /// 1-indexed inclusive start line of the symbol's enclosing range.
    pub start_line: u32,
    /// 1-indexed inclusive end line of the symbol's enclosing range.
    pub end_line: u32,
    pub pagerank_tier: PagerankTier,
    pub change_kind: ChangeKind,
}

/// Result of a `detect_changes` op: a flat list of touched symbols plus
/// a per-file rollup. The from/to shas are echoed back so callers
/// can correlate without re-parsing the request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DetectedChangesResult {
    pub from_sha: String,
    pub to_sha: String,
    pub touched_symbols: Vec<DetectedTouchedSymbol>,
    pub by_file: BTreeMap<String, Vec<DetectedTouchedSymbol>>,
}

/// Result of a `status` query — a peek at the persisted canonical graph cache
/// for a project. No warming side effects.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphStatus {
    pub project_id: String,
    pub warmed: bool,
    pub last_warm_at: Option<String>,
    pub pinned_commit: Option<String>,
    pub commits_since_pin: Option<u64>,
    pub route_parity_enabled: bool,
    pub route_exclusion_config: serde_json::Value,
}

/// One workspace visible to the repository graph, enriched with the latest
/// per-workspace freshness row when one has been persisted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GraphWorkspaceEntry {
    pub slug: String,
    pub name: String,
    pub node_count: usize,
    pub commit_sha: Option<String>,
    pub warmed_at: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkspacesResult {
    pub project_id: String,
    pub workspaces: Vec<GraphWorkspaceEntry>,
}

/// Request for the `crate_graph` bridge operation. Currently empty — the
/// crate graph is always the full workspace view; project context is supplied
/// via `ProjectCtx`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct CrateGraphRequest;

/// A single workspace crate node in the crate-level dependency graph.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrateNodeEntry {
    pub name: String,
    pub manifest_path: String,
    pub loc: usize,
    pub node_count: usize,
    pub fan_in: f64,
    pub fan_out: f64,
    pub inbound_weight: f64,
    pub outbound_weight: f64,
}

/// An aggregated cross-crate edge: source crate → target crate.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrateEdgeEntry {
    pub source: String,
    pub target: String,
    pub weight: f64,
    pub edge_count: usize,
}

/// Full crate-level graph returned by the `crate_graph` bridge operation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CrateGraphResponse {
    pub crates: Vec<CrateNodeEntry>,
    pub edges: Vec<CrateEdgeEntry>,
    /// Present when the graph is empty (e.g. not a Rust workspace) to
    /// communicate the reason without returning an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// PR D2: snapshot node — one entry in the `snapshot.nodes` array. The
/// shape is binding (see `code_graph snapshot` inter-PR contract): `id`
/// is the canonical RepoNodeKey (`"file:..."` / `"symbol:..."`), `kind`
/// is `"file" | "folder" | "symbol"` (folder is reserved for future
/// folder-grouping; D2 emits only `file`/`symbol`), and `pagerank` is
/// the cached score from the canonical-graph ranking.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotNode {
    pub id: String,
    #[serde(default)]
    pub uid: String,
    pub kind: String,
    pub label: String,
    /// Workspace slug that produced this node when available. Optional for
    /// legacy graph artifacts and synthetic/external nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// For community-level snapshots, distinguishes a homogeneous community
    /// (`"single"`) from communities spanning multiple workspace slugs
    /// (`"mixed"`) or nodes without workspace metadata (`"unknown"`). Symbol
    /// snapshots omit this field to preserve the existing wire shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_kind: Option<String>,
    /// Number of graph nodes represented by this snapshot node. Populated for
    /// collapsed community nodes; omitted for symbol/file nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_count: Option<usize>,
    /// Number of original graph edges internal to this community. Populated for
    /// collapsed community nodes; omitted for symbol/file nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal_edge_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    pub pagerank: f64,
    /// Populated post-F3 (Leiden community detection). Always `None` in D2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
    /// Iter 30: per-node cognitive complexity from the tree-sitter
    /// walker (iter 23–25). Only populated for function-like nodes
    /// (Function/Method/Constructor) and only when the file's language
    /// is in the walker's table. `None` for files, types, externals,
    /// synthetic nodes. The UI's `/code-graph` heatmap mode colors by
    /// this field's project-internal percentile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cognitive: Option<u16>,
    /// v10: true when this node is a test (File whose path matches the
    /// test convention, or a Symbol defined in such a file / marked with
    /// the SCIP `Test` role). Mirrors `RepoGraphNode::is_test`. Drives
    /// the `/code-graph` "hide tests" toggle (client-side) and the
    /// `code_graph tests=` server-side filter.
    #[serde(default)]
    pub is_test: bool,
    /// Warm-time layout coordinate (x axis). Populated from the
    /// djinn-graph deterministic community-aware layout cache so the
    /// browser can render static positions without running ForceAtlas2.
    /// Serialized as an explicit field on new snapshots; deserialized
    /// with a `0.0` default so legacy payloads without coordinates are
    /// still accepted at the bridge boundary.
    #[serde(default)]
    pub x: f64,
    /// Warm-time layout coordinate (y axis). See [`SnapshotNode::x`].
    #[serde(default)]
    pub y: f64,
    /// Proposal lmkv: warm-time 3D galaxy layout coordinates. Populated from
    /// the djinn-graph galaxy sidecar (computed once at warm time). `None` on
    /// legacy artifacts that predate the sidecar and for synthetic nodes that
    /// never got a galaxy position — the galaxy UI then falls back to its
    /// client-side worker layout. Skipped on the wire when absent so the Sigma
    /// view and every other snapshot consumer are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gx: Option<f64>,
    /// Galaxy Y coordinate. See [`SnapshotNode::gx`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gy: Option<f64>,
    /// Galaxy Z coordinate. See [`SnapshotNode::gx`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gz: Option<f64>,
    /// Proposal lmkv: per-node degree from the collapsed galaxy edge view,
    /// computed alongside the galaxy positions. Lets the galaxy UI reuse the
    /// server degree instead of recomputing it; `None` on legacy artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degree: Option<u32>,
    /// Keywords extracted from community member names. Populated for
    /// community nodes; empty or omitted for symbol/file nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

/// PR D2: snapshot edge — one entry in the `snapshot.edges` array.
/// `kind` mirrors the `RepoGraphEdgeKind` Debug variant name (matching
/// the `edges` op convention).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// PR D2: full graph snapshot payload — capped with workspace-aware
/// retention and filtered by `graph_excluded_paths`. Wire shape pinned by the
/// inter-PR contract (`code_graph snapshot` section): the entire
/// payload sits under the `snapshot` discriminator field on
/// `CodeGraphResponse`, so it doesn't collide with `Ranked.nodes` or
/// `Edges.edges`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotPayload {
    pub project_id: String,
    pub git_head: String,
    /// ISO8601 UTC timestamp at which the snapshot was assembled.
    pub generated_at: String,
    /// `true` when the underlying graph contained more eligible nodes than
    /// `node_cap`; retention may still promote cross-workspace endpoints.
    pub truncated: bool,
    /// Total node count in the unfiltered, uncapped graph.
    pub total_nodes: usize,
    /// Total edge count in the unfiltered, uncapped graph.
    pub total_edges: usize,
    /// PageRank-tier cap actually applied (default 2000; settable via
    /// the request `limit` field).
    pub node_cap: usize,
    pub nodes: Vec<SnapshotNode>,
    pub edges: Vec<SnapshotEdge>,
}

/// Requested semantic zoom level for a `code_graph snapshot` response.
///
/// `Symbol` is the existing file/symbol node shape. `Community` collapses the
/// graph to one node per stable `Community.id`, with inter-community edges
/// aggregated between those stable ids. The default remains `Symbol` for now so
/// the existing UI is not switched to semantic zoom before its consumer work
/// lands; tests pin the explicit `level=community` contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotLevel {
    Symbol,
    Community,
}

impl SnapshotLevel {
    pub const VALID_VALUES: &'static str = "symbol, community";

    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            None => Ok(Self::Symbol),
            Some(value) if value.eq_ignore_ascii_case("symbol") => Ok(Self::Symbol),
            Some(value) if value.eq_ignore_ascii_case("community") => Ok(Self::Community),
            Some(value) => Err(format!(
                "invalid snapshot level '{value}'; expected one of: {}",
                Self::VALID_VALUES
            )),
        }
    }
}

/// Per-function complexity metrics surfaced on `describe` and
/// `context` responses (iter 27). Wire-shape mirror of
/// `djinn_graph::complexity::ComplexityMetrics`. Computed from the
/// tree-sitter AST during graph build (iter 26); `None` when the
/// symbol's language is outside the walker's table or when the
/// symbol isn't function-like.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, JsonSchema)]
pub struct ComplexityMetrics {
    /// McCabe cyclomatic complexity = 1 + decision-point count.
    pub cyclomatic: u16,
    /// Sonar cognitive complexity. Penalises nesting, flat-rates
    /// `else if`, counts boolean-operator switches.
    pub cognitive: u16,
    /// Non-blank lines inside the body block.
    pub nloc: u16,
    /// Deepest nesting level reached inside the function body.
    pub max_nesting: u8,
    /// Number of formal parameters (includes `self`-receivers).
    pub param_count: u8,
}

/// A symbol description sourced from `ScipSymbol` fields without an LSP round
/// trip.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolDescription {
    pub key: String,
    pub kind: String,
    pub display_name: String,
    pub signature: Option<String>,
    pub documentation: Option<String>,
    pub file: Option<String>,
    /// v8: 1-indexed enclosing range of this symbol's definition in
    /// `file`. `None` for file nodes, synthetic nodes (Process /
    /// Community / Table), and external symbols.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    /// v8: count of incoming dependency edges (Reads/Writes/
    /// SymbolReference/FileReference/Implements/Extends/TypeDefines/
    /// Defines) — i.e. "how many things depend on this". Excludes the
    /// structural anchors (ContainsDefinition/DeclaredInFile) and
    /// synthetic side-channels (MemberOf/StepInProcess/EntryPointOf).
    #[serde(default)]
    pub fan_in: usize,
    /// v8: count of outgoing dependency edges. "How many things this
    /// depends on." Same edge-kind filter as `fan_in`.
    #[serde(default)]
    pub fan_out: usize,
    /// v8: visibility (`public` / `private` / `unknown`) per SCIP
    /// `local`-prefix heuristic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// v8: true when the symbol is from a vendored / third-party
    /// crate / cross-package dep. Discovery ops filter externals by
    /// default; surfacing the flag here lets describe callers reason
    /// about why a symbol they're examining might not show up in
    /// ranked / orphans / dead.
    #[serde(default)]
    pub is_external: bool,
    /// v8: true when this symbol has any incoming `EntryPointOf`
    /// edge (i.e. the entry-point detector flagged it as `fn main`,
    /// a route handler, a test, etc.).
    #[serde(default)]
    pub is_entry_point: bool,
    /// v8: SCIP-marked Test role. Mirrors the `is_test` flag on the
    /// underlying graph node.
    #[serde(default)]
    pub is_test: bool,
    /// Iter 27: per-function complexity metrics from the tree-sitter
    /// walker. `None` for non-function symbols, file/synthetic nodes,
    /// and languages outside the walker's table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<ComplexityMetrics>,
}

/// Per-file rollup of `impact`/`neighbors` results, returned when
/// `group_by="file"` is set.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileGroupEntry {
    pub file: String,
    pub occurrence_count: usize,
    pub max_depth: usize,
    pub sample_keys: Vec<String>,
}

/// An impact-set entry: a node transitively dependent on the queried node.
///
/// `file_path` (PR C3): the relative file path of the impacted node when
/// it is known. Carried alongside the SCIP key so the response-shaping
/// layer can bucket entries into modules for risk classification without
/// re-resolving the graph node. `None` for nodes that lack a `file_path`
/// (e.g. external/virtual symbols).
///
/// `confidence_tier` (PR s6ch / 92z7): the edge confidence tier of the
/// last hop used to reach this node — `"extracted"`, `"inferred"`, or
/// `"ambiguous"`. Surfacing it on the entry lets the UI label inferred
/// consumer routes as suggestions without a follow-up call.
///
/// `exclusion_reason` (PR s6ch / 92z7): set when the impact BFS would
/// have reached this node but the route-exclusion policy classified
/// the link as a non-blast-radius suggestion
/// (`"below-confidence-floor"`, `"health-path"`, `"param-only-path"`).
/// The entry is still returned so the UI can display it as a soft
/// dependency instead of a hard one. `None` for entries whose inbound
/// edge passed every filter.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactEntry {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
}

/// Either symbol-level neighbors/impact or per-file rollup.
/// A single public-surface entry emitted by the `api_surface` op.
///
/// Enriches each symbol with its fan-in/fan-out and a "used outside its
/// own crate" flag so callers can reason about which exports are actually
/// consumed by downstream crates vs. internal-only API.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ApiSurfaceEntry {
    pub key: String,
    pub display_name: String,
    pub symbol_kind: Option<String>,
    pub file: Option<String>,
    pub visibility: Option<String>,
    /// Whether the symbol's SCIP `documentation` field has at least one
    /// non-empty line.
    pub doc_present: bool,
    pub fan_in: usize,
    pub fan_out: usize,
    /// True when at least one incoming edge's source node lives in a
    /// different crate than this symbol. Derived from the SCIP key's
    /// `<tool> <scheme> <crate-name> <version> ...` preamble.
    pub used_outside_crate: bool,
}

/// A single boundary-check rule — a pair of globs. Every rule is
/// treated as a forbidden edge; callers submit only the rules they want
/// flagged as violations.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct BoundaryRule {
    pub from_glob: String,
    pub to_glob: String,
    /// Optional human-readable explanation of why this rule exists,
    /// surfaced in CI output so violations are self-documenting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A single violation emitted by the `boundary_check` op.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BoundaryViolation {
    /// Index of the rule in the caller's input array.
    pub rule_index: usize,
    pub from_key: String,
    pub to_key: String,
    pub edge_kind: String,
    pub from_file: Option<String>,
    pub to_file: Option<String>,
    /// V1: set to `Some(vec![from_key, to_key])` — the direct edge is
    /// the witness. Multi-hop transitive witnessing is deferred.
    pub witness_path: Option<Vec<String>>,
}

/// Iter 28: result emitted by the `complexity` op. Untagged so the
/// per-target shape ships under a single discriminator-free union; the
/// `Functions` variant is the default `target=functions` payload, and
/// `Files` is the `target=files` aggregation.
///
/// JsonSchema: untagged enums of homogeneous variants serialize as a
/// `oneOf` of the inner array shapes — matches the contract `code_graph`
/// uses for `NeighborsResult` and `ImpactResult`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ComplexityResult {
    Functions(Vec<FunctionComplexityEntry>),
    Files(Vec<FileComplexityEntry>),
}

/// Per-function entry for the `complexity` op (target=functions). The
/// `metrics` payload is the same wire shape `describe`/`context` ship
/// (iter 27); the extra fields lift the function's location so callers
/// don't need a follow-up `describe` to know where to look.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FunctionComplexityEntry {
    pub key: String,
    pub display_name: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub metrics: ComplexityMetrics,
}

/// Per-file aggregation for the `complexity` op (target=files). Sums
/// every function-like node's metrics by their `file_path` and tracks
/// the worst offender (`max_function_*`) so callers can sort by either
/// totals or peaks.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileComplexityEntry {
    pub file: String,
    pub function_count: u32,
    pub total_cognitive: u32,
    pub total_cyclomatic: u32,
    pub total_nloc: u32,
    pub max_function_cognitive: u16,
    pub max_function_name: String,
}

/// Iter 29: a single refactor-candidate entry emitted by the
/// `refactor_candidates` op. Composite ranking that fuses three
/// individually-noisy signals (cognitive complexity, file-level churn,
/// PageRank) via z-score averaging. Higher `composite_score` = more
/// urgent refactor target.
///
/// The raw signal fields (`cognitive`, `cyclomatic`, `churn_commits`,
/// `page_rank`) and the per-axis z-scores are surfaced alongside the
/// composite so callers can re-rank in their own UI without a
/// round-trip back through `code_graph`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefactorCandidate {
    pub key: String,
    #[serde(default)]
    pub uid: String,
    pub display_name: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Composite score = mean of the three z-scores. Higher = more
    /// urgent refactor target.
    pub composite_score: f64,
    /// Tier label: `"high"` (top 10% of the returned set), `"medium"`
    /// (next 15%), `"low"` (rest). For result sets with fewer than 10
    /// candidates every entry is `"high"` (degenerate small project).
    pub tier: String,
    pub cognitive: u16,
    pub cyclomatic: u16,
    /// File-level commit count over the `since_days` window. Functions
    /// in the same file share this number; functions whose file isn't
    /// in the churn map get `0`.
    pub churn_commits: u32,
    pub page_rank: f64,
    pub z_cognitive: f64,
    pub z_churn: f64,
    pub z_page_rank: f64,
}

/// A single hotspot entry emitted by `hotspots`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HotspotEntry {
    pub file: String,
    /// Distinct commits in the window that touched this file.
    pub churn: usize,
    /// Sum of PageRank over every symbol node whose `file_path` is this file.
    pub centrality: f64,
    /// `churn * centrality`.
    pub composite_score: f64,
    /// Up to three display names of the highest-PageRank symbols in the file.
    pub top_symbols: Vec<String>,
}

/// Scalar graph snapshot emitted by `metrics_at`. Reflects the
/// currently-pinned canonical graph commit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MetricsAtResult {
    /// The canonical commit these metrics pertain to.
    pub commit: String,
    pub node_count: usize,
    pub edge_count: usize,
    /// All-scopes cycle count. INCLUDES tautological file↔symbol
    /// 2-cycles (every symbol forms one with its containing file via
    /// ContainsDefinition + DeclaredInFile). For "real" code-level
    /// cycles use `cycle_count_symbol_only` (matches what the
    /// `cycles` op returns by default).
    pub cycle_count: usize,
    /// v8: cycle count over the symbol-only subgraph — matches the
    /// `cycles(kind_filter="symbol")` op (the `cycles` op default).
    /// This is the architecturally meaningful number; the `cycle_count`
    /// total above is the all-scopes raw count.
    #[serde(default)]
    pub cycle_count_symbol_only: usize,
    /// v8: cycle count over the file-only subgraph (file→file import
    /// cycles). Only relevant for languages whose import graph is
    /// non-DAG (Go, Python, Ruby).
    #[serde(default)]
    pub cycle_count_file_only: usize,
    /// Histogram bucketing all-scopes SCCs by member count.
    pub cycles_by_size_histogram: BTreeMap<usize, usize>,
    pub god_object_count: usize,
    pub orphan_count: usize,
    pub public_api_count: usize,
    pub doc_coverage_pct: f64,
}

/// A single dead-symbol entry emitted by `dead_symbols`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeadSymbolEntry {
    pub key: String,
    pub display_name: String,
    pub symbol_kind: Option<String>,
    pub file: Option<String>,
    pub visibility: Option<String>,
    /// Echoed from the caller's `confidence` argument (`"high"`, `"med"`, `"low"`).
    pub confidence: String,
}

/// A single deprecated-symbol hit plus its callers, emitted by
/// `deprecated_callers`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeprecatedHit {
    pub deprecated_symbol: String,
    pub deprecated_display_name: String,
    pub deprecated_file: Option<String>,
    pub callers: Vec<CallerRef>,
}

/// Caller reference pointed at by [`DeprecatedHit`].
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CallerRef {
    pub key: String,
    pub display_name: String,
    pub file: Option<String>,
}

/// A single co-edit peer emitted by `coupling`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingEntry {
    pub file_path: String,
    /// Number of distinct commits that touched both files.
    pub co_edit_count: usize,
    /// ISO-8601 UTC timestamp of the most recent co-edit.
    pub last_co_edit: String,
    /// Up to three sample SHAs from the supporting commits,
    /// newest-first — lets the caller jump straight to a diff for
    /// context.
    pub supporting_commit_samples: Vec<String>,
}

/// A single file-pair hit emitted by `coupling_hotspots`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoupledPairEntry {
    pub file_a: String,
    pub file_b: String,
    pub co_edits: usize,
    /// ISO-8601 UTC timestamp of the most recent commit that touched
    /// both files.
    pub last_co_edit: String,
}

/// A single coupling-hub hit emitted by `coupling_hubs`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CouplingHubEntry {
    pub file_path: String,
    /// Sum of `co_edits` across every pair the file participates in.
    pub total_coupling: usize,
    /// Number of distinct files this file has been co-edited with.
    pub partner_count: usize,
}

/// A single churn row emitted by `churn`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChurnEntry {
    pub file_path: String,
    /// Distinct commits that touched the file in the selected window.
    pub commit_count: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// ISO-8601 UTC timestamp of the most recent commit that touched
    /// the file in the selected window.
    pub last_commit_at: String,
}

/// PR C1: edge categories used to bucket incoming/outgoing neighbors in
/// the `context` op response. Mirrors the inter-PR contract table mapping
/// `RepoGraphEdgeKind` → category. Serialized as snake_case so JSON keys
/// like `calls`, `reads`, `type_defines` line up with the UI parsers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeCategory {
    /// SymbolReference where the target symbol is a function/method/constructor.
    /// Also includes synthesized `TraitDispatchCall` edges (epic ggrm/5wyo),
    /// which represent trait-dispatch callers resolved through canonical
    /// fan-out. Use the per-neighbor `confidence` / `confidence_tier` to
    /// distinguish lower-confidence trait-dispatch edges from directly
    /// extracted calls.
    Calls,
    /// SymbolReference catch-all (imports, type-only references, etc.).
    References,
    /// FileReference — file-to-file edge derived from cross-file occurrences.
    Imports,
    /// ContainsDefinition / DeclaredInFile — file ↔ symbol containment.
    Contains,
    /// `RepoGraphEdgeKind::Extends` — subtype-of / extends.
    Extends,
    /// `RepoGraphEdgeKind::Implements` — interface / trait implementation.
    Implements,
    /// `RepoGraphEdgeKind::TypeDefines` — variable / param / return type,
    /// type alias target, generic bound.
    TypeDefines,
    /// `RepoGraphEdgeKind::Defines` — canonical-definition relationship
    /// (target's defining region is contained in the source).
    Defines,
    /// PR A3: SymbolRole::ReadAccess split-out.
    Reads,
    /// PR A3: SymbolRole::WriteAccess split-out.
    Writes,
    /// PR F1: `EntryPointOf` — file → symbol metadata edge stamped by
    /// the entry-point detector. Surfaced as its own category so the UI
    /// can render an "entry point" badge on the symbol panel without
    /// confusing it with structural call / reference edges.
    EntryPoint,
    /// PR F2: `StepInProcess` — synthetic edge from a `Process` node
    /// to each step along a traced execution flow. Surfaced as its
    /// own category so the UI can group process-membership edges
    /// separately from real call / reference edges. Note: this only
    /// shows up on `incoming` for symbol nodes (whose ancestor in the
    /// edge is the synthetic process node).
    Process,
}

/// PR C1: a neighbor of the queried symbol, grouped under its
/// [`EdgeCategory`] in [`SymbolContext::incoming`] / `outgoing`. The shape
/// mirrors [`GraphNeighbor`] but carries the category-aware view used by
/// the 360° symbol panel.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RelatedSymbol {
    /// Stable RepoNodeKey (`"symbol:..."` or `"file:..."`). Pass back as
    /// `key` for follow-up `context` / `impact` calls.
    pub uid: String,
    /// Display name (typically the unqualified identifier).
    pub name: String,
    /// `"file"`, `"function"`, `"class"`, `"method"`, etc.
    pub kind: String,
    /// Repository-relative file path when known. `None` for symbol nodes
    /// that lack a `file_path` (synthetic placeholders, externals).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Confidence carried by the underlying edge (PR A2 — propagates to
    /// the UI so weak references can be visually de-emphasized).
    pub confidence: f64,
    /// Model-level confidence tier derived from the underlying graph edge.
    /// Stable snake_case string: `extracted`, `inferred`, or `ambiguous`.
    pub confidence_tier: String,
    /// Human-readable confidence/exclusion explanation carried by route-aware
    /// inferred edges (for example `ts-fetch-literal` or
    /// `below-confidence-floor`). Present when the underlying graph edge has a
    /// reason so callers can audit why a consumer link was included or
    /// excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_reason: Option<String>,
    /// Route-exclusion reason for audit-only route/consumer links. `None` means
    /// the symbol participates in the default blast radius; `Some(...)` means it
    /// was suppressed by the configured route exclusion policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<String>,
    /// Compat-safe audit metadata for route consumer edges. Present for
    /// `Fetches`/route links so UI/API consumers can display the language chain
    /// (for example TypeScript → Rust) without changing persisted graph edges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_language_chain: Option<RouteLanguageChain>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RouteLanguageChain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_language: Option<String>,
    pub is_cross_language: bool,
}

/// PR C1: structured method metadata. Populated only when the upstream
/// SCIP indexer emits structured signature fields; absent otherwise — the
/// plan explicitly forbids regexing the markdown signature blob.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct MethodMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_async: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<MethodParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<String>,
}

/// PR C1: a single parameter on a method/function symbol. Lifted from
/// the structured `scip::Signature` proto when the indexer populates it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MethodParam {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
}

/// PR C1: stub for Epic F2's "process" linking. A `Context` response
/// carries an empty `processes: []` list until F2 backfills the
/// process-membership index. The shape is fixed up-front so UI
/// consumers can render the empty list today and progressive-enhance
/// later.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessRef {
    pub id: String,
    #[serde(default)]
    pub uid: String,
    pub label: String,
    pub role: String,
}

/// PR C1: the queried symbol's identity + content + structural metadata
/// returned in [`SymbolContext::symbol`].
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolNode {
    /// Stable RepoNodeKey of the queried node.
    pub uid: String,
    /// Display name (unqualified identifier, file basename, etc.).
    pub name: String,
    /// `"file"`, `"function"`, `"class"`, `"method"`, etc.
    pub kind: String,
    /// Repository-relative file path. Empty string for synthetic nodes.
    pub file_path: String,
    /// 1-indexed inclusive start line of the definition range. `0` when
    /// the indexer didn't pin a line range to the symbol.
    pub start_line: u32,
    /// 1-indexed inclusive end line of the definition range. `0` when
    /// no range is known.
    pub end_line: u32,
    /// Body text — only populated when the caller passes
    /// `include_content=true`. Bandwidth-gated by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Structured method metadata when SCIP populated it. `None` for
    /// non-method symbols and for indexers that only emit the markdown
    /// signature blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method_metadata: Option<MethodMeta>,
    /// Iter 27: per-function complexity metrics from the tree-sitter
    /// walker. `None` for non-function symbols, file/synthetic nodes,
    /// and languages outside the walker's table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<ComplexityMetrics>,
}

/// PR C1: 360° view of a single symbol — the queried node plus its
/// categorized incoming/outgoing neighbors and (post-F2) the process
/// memberships the symbol participates in.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolContext {
    pub symbol: SymbolNode,
    /// Incoming neighbors bucketed by [`EdgeCategory`]. Each bucket is
    /// hard-capped at 30 entries (per the plan) so the wire payload
    /// stays bounded on high-fan-in symbols.
    pub incoming: BTreeMap<EdgeCategory, Vec<RelatedSymbol>>,
    /// Outgoing neighbors bucketed by [`EdgeCategory`]. Same 30-entry cap.
    pub outgoing: BTreeMap<EdgeCategory, Vec<RelatedSymbol>>,
    /// F2 stub — empty until process membership lands.
    pub processes: Vec<ProcessRef>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RouteRef {
    pub uid: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RouteSummary {
    pub total_routes: usize,
    pub framework_counts: BTreeMap<String, usize>,
    pub handler_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RouteMapEntry {
    pub route: RouteRef,
    /// Populated when the configured route-exclusion policy suppresses this
    /// route from route-aware default analyses (health/ping endpoints,
    /// param-only paths, excluded frameworks, etc.). The entry remains visible
    /// in `route_map` so callers can audit the exclusion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<RelatedSymbol>,
    pub middleware: Vec<RelatedSymbol>,
    pub consumers: Vec<RelatedSymbol>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RouteMapResult {
    pub routes: Vec<RouteMapEntry>,
    pub summary: RouteSummary,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ShapeField {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct RouteShape {
    pub route: RouteRef,
    pub response_fields: Vec<ShapeField>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ShapeTypeMismatch {
    pub key: String,
    pub server_type: String,
    pub consumer_type: String,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ShapeDrift {
    pub consumer: RelatedSymbol,
    pub missing_keys: Vec<String>,
    pub extra_keys: Vec<String>,
    pub type_mismatches: Vec<ShapeTypeMismatch>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ShapeCheckResult {
    /// Whether the selector matched a route that was eligible for default
    /// shape-checking after applying route exclusions.
    pub matched: bool,
    /// Truthful one-line status for empty/excluded/unavailable extraction cases.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<String>,
    pub route_shape: RouteShape,
    pub drifts: Vec<ShapeDrift>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ApiImpactEntry {
    pub consumer: RelatedSymbol,
    pub risk_tier: String,
    pub reason: String,
    /// PR s6ch / 92z7: machine-readable exclusion reason when the
    /// active route policy classified the consumer's inbound edge
    /// as a non-blast-radius suggestion. Drives the UI's "soft
    /// dependency" treatment of inferred routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct ApiImpactResult {
    pub impacts: Vec<ApiImpactEntry>,
    /// Audit-only entries excluded from the default blast radius by route
    /// exclusion policy or below-floor `Fetches` confidence. Kept separate so
    /// callers can inspect weak suggestions without treating them as impact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_impacts: Vec<ApiImpactEntry>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowHit {
    pub process: ProcessRef,
    pub matched_step: RelatedSymbol,
    pub matched_step_index: i32,
    pub rrf_score: f64,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct FlowResult {
    pub hits: Vec<FlowHit>,
}

/// A ranked disambiguation candidate emitted by the `code_graph`
/// `resolve` op (PR C2). When `code_graph` cannot resolve a caller-supplied
/// key (`User`, `helper`, `MyClass`) to a single graph node, the dispatcher
/// falls back to `search_by_name` and returns up to 8 ranked `Candidate`s
/// instead of a hard error.
///
/// `uid` is the stable `RepoNodeKey` (`"symbol:..."` or `"file:..."`) — a
/// follow-up call with `key=<uid>` resolves uniquely.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Candidate {
    /// Stable RepoNodeKey, e.g. `"symbol:scip-rust pkg src/foo.rs `User`#"`.
    /// Pass back as `key` for an unambiguous follow-up.
    pub uid: String,
    /// Display name (typically the unqualified identifier).
    pub name: String,
    /// `"file"`, `"function"`, `"class"`, `"method"`, `"interface"`, etc.
    pub kind: String,
    /// Repository-relative file path, when known. Empty string for
    /// symbol nodes that don't carry a `file_path`.
    pub file_path: String,
    /// Composite ranking score from PR C2's formula:
    /// `0.5 + 0.4 * file-path-match + 0.2 * kind-hint-match + tiebreaker`.
    pub score: f64,
}

/// Outcome of pre-resolving a `code_graph` key against the live graph.
/// Surfaces multi-match cases as `Ambiguous` so callers can show a
/// disambiguation UI instead of failing the whole tool call.
#[derive(Debug, Clone)]
pub enum ResolveOutcome {
    /// Exact match landed on a unique node. The contained `String` is
    /// the canonical RepoNodeKey (`"symbol:..."` or `"file:..."`).
    Found(String),
    /// Exact match failed; `search_by_name` returned multiple plausible
    /// targets. Up to 8, ranked by the PR C2 formula.
    Ambiguous(Vec<Candidate>),
    /// No exact match and no name-index hits.
    NotFound,
}

/// A single hot-path hit emitted by `touches_hot_path`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HotPathHit {
    pub symbol: String,
    /// Number of entry→sink pairs whose shortest path includes `symbol`.
    pub on_path_count: usize,
    /// One example path containing `symbol` (entry → … → sink).
    pub example_path: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum NeighborsResult {
    Detailed(Vec<GraphNeighbor>),
    Grouped(Vec<FileGroupEntry>),
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ImpactResult {
    Detailed(Vec<ImpactEntry>),
    Grouped(Vec<FileGroupEntry>),
}

/// Resolved project handle passed to every `RepoGraphOps` call.
///
/// Built once in the `code_graph` / `pr_review_context` dispatch from
/// the incoming project ref (UUID or `"owner/repo"` slug). Carries:
/// - `id`: UUIDv7 project identifier — the key for `repo_graph_cache`
///   and other per-project tables.
/// - `clone_path`: `$DJINN_HOME/projects/{owner}/{repo}` — the
///   filesystem root the SCIP indexer / git CLI operates against.
/// - `workspace`: optional workspace slug supplied by the caller after
///   request-boundary normalization.
/// - `sub_path`: optional repository-relative workspace sub-path, when
///   available to the dispatch caller.
///
/// Every bridge method takes this by reference so implementations can
/// decide whether an operation is DB-only (`status`, `metrics_at`) or
/// filesystem-touching (`hotspots` / `diff_touches` → git log / diff)
/// without re-resolving anything.
#[derive(Clone, Debug)]
pub struct ProjectCtx {
    pub id: String,
    pub clone_path: String,
    pub workspace: Option<String>,
    pub sub_path: Option<String>,
}

/// Resolved workspace scope for a `RepoGraphOps` call (pb94 epic).
///
/// Built by the `code_graph` dispatcher (and chat-side callers) before
/// invoking a listing/bounded/traversal op. The single source of truth for
/// the three workspace-parameter outcomes the epic commits to:
/// - **Empty / unscoped** — caller omitted `workspace` or sent `""`
///   (already normalized to `None` upstream). `workspace = None`,
///   `hint = None`. Bridge runs over the full graph.
/// - **Valid / known** — requested slug is non-empty and the graph has at
///   least one node (or zero nodes) in that workspace. `workspace = Some(slug)`,
///   `hint = None`. Single-workspace graphs land here too — the workspace
///   filter is a structural no-op (matches every node) rather than a
///   surprising hard-empty result.
/// - **Unknown non-empty slug** — requested slug is non-empty and the graph
///   contains no node in that workspace. `workspace = None` (so the bridge
///   returns the full result), `hint = Some(candidates)` for the caller to
///   recover. Single-candidate hint lists are suppressed — when the project
///   exposes exactly one workspace, "I don't know `foo`, did you mean `server`?"
///   is helpful only if `server` is genuinely a choice.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceScope {
    /// Effective workspace slug to hand to the bridge call. `None` for
    /// unscoped, unknown, or single-workspace-no-op cases.
    pub workspace: Option<String>,
    /// Available workspace candidates to surface to the caller. Set
    /// only when the requested slug was non-empty AND unknown AND the
    /// project exposes more than one workspace.
    pub hint: Option<Vec<String>>,
}
