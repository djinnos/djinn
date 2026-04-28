/// Bridge traits that decouple djinn-control-plane from the server binary.
///
/// The server implements each trait for its concrete handle types
/// (CoordinatorHandle, SlotPoolHandle, LspManager, AppState).
/// McpState holds Arc<dyn Trait> so the MCP layer never imports server types.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct SemanticQueryEmbedding {
    pub values: Vec<f32>,
}

// ── Data types ─────────────────────────────────────────────────────────────────
// Plain data returned by the bridge traits. Defined here so they can be used
// by both djinn-control-plane tool handlers and the server bridge implementations.

#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    /// Tasks merged per hour per epic in the past hour (epic_id → count).
    pub epic_throughput: HashMap<String, usize>,
    /// Per-project PR creation errors (e.g. org OAuth restrictions).
    pub pr_errors: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ModelPoolStatus {
    pub active: u32,
    pub free: u32,
    pub total: u32,
}

#[derive(Debug, Clone)]
pub struct RunningTaskInfo {
    pub task_id: String,
    pub model_id: String,
    pub slot_id: usize,
    pub duration_seconds: u64,
    pub idle_seconds: u64,
    /// Project UUID the task belongs to, tracked by the slot pool so
    /// project-scoped status queries can filter pre-session lifecycles.
    pub project_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub active_slots: usize,
    pub total_slots: usize,
    pub per_model: HashMap<String, ModelPoolStatus>,
    pub running_tasks: Vec<RunningTaskInfo>,
}

#[derive(Debug, Clone)]
pub struct LspWarning {
    pub server: String,
    pub message: String,
}

// ── Coordinator ─────────────────────────────────────────────────────────────────

#[async_trait]
pub trait CoordinatorOps: Send + Sync {
    fn get_status(&self) -> Result<CoordinatorStatus, String>;
    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String>;
}

// ── Slot pool ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SlotPoolOps: Send + Sync {
    async fn get_status(&self) -> Result<PoolStatus, String>;
    async fn kill_session(&self, task_id: &str) -> Result<(), String>;
    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String>;
    async fn has_session(&self, task_id: &str) -> Result<bool, String>;
}

// ── LSP ─────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LspOps: Send + Sync {
    async fn warnings(&self) -> Vec<LspWarning>;
}

// ── Runtime ─────────────────────────────────────────────────────────────────────
// Server-level operations that don't fit neatly into the other trait groups.

#[async_trait]
pub trait RuntimeOps: Send + Sync {
    async fn apply_settings(
        &self,
        settings: &djinn_core::models::DjinnSettings,
    ) -> Result<(), String>;
    async fn embed_memory_query(
        &self,
        query: &str,
    ) -> Result<Option<SemanticQueryEmbedding>, String>;
    async fn reset_runtime_settings(&self);
    async fn persist_model_health_state(&self);
    /// P6: persist a fresh `EnvironmentConfig` for a project + upsert
    /// the runtime ConfigMap + null `image_hash` so the next tick
    /// rebuilds. In dev mode without a kube client, falls back to a
    /// plain DB write. `config_json` is the serialised `EnvironmentConfig`
    /// — caller has already validated.
    async fn apply_environment_config(
        &self,
        project_id: &str,
        config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String>;
}

// ── Git ─────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait GitOps: Send + Sync {
    async fn git_actor(
        &self,
        path: &Path,
    ) -> Result<djinn_git::GitActorHandle, djinn_git::GitError>;
}

// ── Repo Graph ──────────────────────────────────────────────────────────────────
// Bridge for RepoDependencyGraph queries. The server implements this by
// building the graph from SCIP artifacts; djinn-control-plane/djinn-agent never depend
// on petgraph or SCIP protobuf types directly.

/// A neighbor of a node in the repository dependency graph.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphNeighbor {
    pub key: String,
    pub kind: String,
    pub display_name: String,
    pub edge_kind: String,
    pub edge_weight: f64,
    pub direction: String,
}

/// A ranked node from PageRank + structural weight scoring.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RankedNode {
    pub key: String,
    pub kind: String,
    pub display_name: String,
    pub score: f64,
    pub page_rank: f64,
    pub structural_weight: f64,
    pub inbound_edge_weight: f64,
    pub outbound_edge_weight: f64,
}

/// A search hit from the name-index lookup or hybrid RRF fusion. Returned
/// by `search`. PR B4 added `match_kind` (which signal contributed the
/// hit — `"name"` / `"lexical"` / `"semantic"` / `"structural"` / `"hybrid"`)
/// for debug / Pulse-panel surfaces; old clients that don't read it stay
/// unaffected because the field is `skip_serializing_if = "Option::is_none"`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchHit {
    pub key: String,
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
    pub kind: String,
    pub display_name: String,
    pub file: Option<String>,
    pub visibility: String,
}

/// A single hop in a `path` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PathHop {
    pub key: String,
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
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    pub pagerank: f64,
    /// Populated post-F3 (Leiden community detection). Always `None` in D2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
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

/// PR D2: full graph snapshot payload — capped by PageRank tier and
/// filtered by `graph_excluded_paths`. Wire shape pinned by the
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
    /// `true` when the underlying graph contained more nodes than
    /// `node_cap`, so the lowest-PageRank tier was dropped.
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
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactEntry {
    pub key: String,
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
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
    pub cycle_count: usize,
    /// Histogram bucketing SCCs by member count.
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
    Calls,
    /// SymbolReference catch-all (imports, type-only references, etc.).
    References,
    /// FileReference — file-to-file edge derived from cross-file occurrences.
    Imports,
    /// ContainsDefinition / DeclaredInFile — file ↔ symbol containment.
    Contains,
    /// SymbolRelationshipReference — subtype-of / extends.
    Extends,
    /// SymbolRelationshipImplementation.
    Implements,
    /// SymbolRelationshipTypeDefinition.
    TypeDefines,
    /// SymbolRelationshipDefinition.
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
#[derive(Debug, Clone, Serialize, JsonSchema)]
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
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProcessRef {
    pub id: String,
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
///
/// Every bridge method takes this by reference so implementations can
/// decide whether an operation is DB-only (`status`, `metrics_at`) or
/// filesystem-touching (`hotspots` / `diff_touches` → git log / diff)
/// without re-resolving anything.
#[derive(Clone, Debug)]
pub struct ProjectCtx {
    pub id: String,
    pub clone_path: String,
}

#[async_trait]
pub trait RepoGraphOps: Send + Sync {
    /// Neighbors of a file or symbol node (edges in/out). When `group_by` is
    /// `Some("file")`, results are collapsed into per-file rollups.
    ///
    /// `kind_filter` (PR A3) restricts the response to neighbors reached by
    /// edges of a specific kind: `Some("reads")` keeps only `Reads` edges,
    /// `Some("writes")` only `Writes`. `None` keeps every kind (the
    /// pre-PR-A3 behaviour).
    async fn neighbors(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
        kind_filter: Option<&str>,
    ) -> Result<NeighborsResult, String>;

    /// Top-ranked nodes by PageRank + structural weight. `sort_by` can be one
    /// of `pagerank` (default), `in_degree`, `out_degree`, or `total_degree`.
    async fn ranked(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        sort_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedNode>, String>;

    /// Symbols that implement a given trait/interface symbol.
    async fn implementations(
        &self,
        ctx: &ProjectCtx,
        symbol: &str,
    ) -> Result<Vec<String>, String>;

    /// Transitive impact set — nodes that depend on the queried node. When
    /// `group_by` is `Some("file")`, results are collapsed into per-file
    /// rollups.
    ///
    /// `min_confidence` filters the BFS frontier: edges whose
    /// [`djinn_graph::repo_graph::RepoGraphEdge::confidence`] falls below the
    /// threshold are skipped, so weak SCIP signals (e.g. `local`-prefixed
    /// references that took the visibility-heuristic penalty) drop out of the
    /// blast radius. `None` keeps every edge — the pre-PR-A2 behaviour.
    async fn impact(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        depth: usize,
        group_by: Option<&str>,
        min_confidence: Option<f64>,
    ) -> Result<ImpactResult, String>;

    /// Name-based symbol search.
    async fn search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String>;

    /// PR B4: hybrid lexical + semantic + structural search via RRF
    /// fusion (k=60). The bridge implementation orchestrates the three
    /// signals (lexical = SQL `LIKE` over `code_chunks.embedded_text`,
    /// semantic = Qdrant cosine over the `code_chunks` collection,
    /// structural = `search_by_name` against the canonical graph),
    /// caps each signal at top-3 chunks per file, fuses the resulting
    /// rankings, and stamps each hit's `match_kind` for debug surfaces.
    ///
    /// Default impl falls back to [`Self::search`] so test stubs that
    /// only care about the structural signal don't have to plumb the
    /// hybrid pipeline. Production wires this on the server side via
    /// `RepoGraphBridge` (`server/src/mcp_bridge.rs`).
    async fn hybrid_search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        // Default: degrade to the structural-only path. This keeps the
        // trait surface backwards-compatible for stubs while letting
        // production override with the full RRF orchestrator.
        let mut hits = self.search(ctx, query, kind_filter, limit).await?;
        for hit in hits.iter_mut() {
            hit.match_kind = Some("structural".to_string());
        }
        Ok(hits)
    }

    /// Strongly-connected components of size >= `min_size`.
    async fn cycles(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        min_size: usize,
    ) -> Result<Vec<CycleGroup>, String>;

    /// Bulk dead-symbol enumeration (nodes with zero incoming references).
    async fn orphans(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String>;

    /// Shortest dependency path between two nodes.
    async fn path(
        &self,
        ctx: &ProjectCtx,
        from: &str,
        to: &str,
        max_depth: Option<usize>,
    ) -> Result<Option<PathResult>, String>;

    /// Enumerate edges matching path globs.
    async fn edges(
        &self,
        ctx: &ProjectCtx,
        from_glob: &str,
        to_glob: &str,
        edge_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EdgeEntry>, String>;

    /// Detailed description of a single symbol.
    async fn describe(
        &self,
        ctx: &ProjectCtx,
        key: &str,
    ) -> Result<Option<SymbolDescription>, String>;

    /// PR C1: 360° view of a symbol — resolved node identity plus
    /// categorized incoming/outgoing neighbors. Each category list is
    /// hard-capped at 30 entries server-side. When `include_content` is
    /// `true`, [`SymbolNode::content`] is populated with the symbol's
    /// body text (best-effort: requires the file to be readable from the
    /// project clone). The `processes` list is empty until F2 backfills
    /// process membership.
    async fn context(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        include_content: bool,
    ) -> Result<Option<SymbolContext>, String>;

    /// Peek at the in-memory canonical graph cache for the given project.
    /// MUST NOT trigger any warming or SCIP indexing.  When the cache is
    /// empty for this project, returns `warmed: false` with the timestamp/
    /// commit fields set to `None`.
    async fn status(&self, ctx: &ProjectCtx) -> Result<GraphStatus, String>;

    /// PR D2: full-graph snapshot capped by PageRank tier — the wire
    /// payload that drives the `/code-graph` UI's Sigma render. The
    /// caller passes a `node_cap` (default 2000); we keep the top
    /// `node_cap` nodes by PageRank, then emit every edge whose source
    /// AND target survived the cap. `excluded_keys` is the pre-resolved
    /// set of node keys filtered out by `graph_excluded_paths`; both
    /// node and edge filtering happens against this set so the wire
    /// shape is consistent with what the rest of `code_graph` returns.
    async fn snapshot(
        &self,
        ctx: &ProjectCtx,
        node_cap: usize,
        exclusions: &crate::tools::graph_exclusions::GraphExclusions,
    ) -> Result<SnapshotPayload, String>;

    /// Resolve a `(file, start_line, end_line?)` tuple into the set of
    /// base-graph symbols whose definition range encloses the queried
    /// lines. Used for diff-hunk → symbol mapping during PR review.
    async fn symbols_at(
        &self,
        ctx: &ProjectCtx,
        file: &str,
        start_line: u32,
        end_line: Option<u32>,
    ) -> Result<Vec<SymbolAtHit>, String>;

    /// Map a list of changed line ranges (parsed from
    /// `git diff --unified=0 base..head`) to the set of base-graph
    /// symbols they touch, with fan-in/fan-out and file grouping.
    ///
    /// Runs entirely against the already-warmed canonical graph on the
    /// project's base branch — it does NOT build a head graph.
    async fn diff_touches(
        &self,
        ctx: &ProjectCtx,
        changed_ranges: &[ChangedRange],
    ) -> Result<DiffTouchesResult, String>;

    /// Given a SHA range (`from_sha..to_sha`) or an explicit
    /// `changed_files` list, return every symbol whose enclosing range
    /// overlaps a hunk, bucketed by current-project PageRank tier.
    ///
    /// SHA-range mode runs `git diff --unified=0 from..to` against the
    /// project clone and pipes the hunks through
    /// `RepoDependencyGraph::symbols_enclosing`. The `changed_files`
    /// fallback considers every symbol in the listed files as touched
    /// (no line-level filtering). Both modes can be combined; line-level
    /// wins.
    ///
    /// PageRank tiers are quartile-bucketed against the current
    /// project graph at request time, NOT a graph rebuilt at the from
    /// or to sha — review weight reflects "what matters now."
    async fn detect_changes(
        &self,
        ctx: &ProjectCtx,
        from_sha: Option<&str>,
        to_sha: Option<&str>,
        changed_files: &[String],
    ) -> Result<DetectedChangesResult, String>;

    /// List every public (or private/any, per `visibility`) symbol in
    /// the base graph, enriched with fan-in / fan-out and a
    /// "used outside crate" signal.
    async fn api_surface(
        &self,
        ctx: &ProjectCtx,
        module_glob: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApiSurfaceEntry>, String>;

    /// Match edges whose source matches `from_glob` AND target matches
    /// `to_glob`, returning the forbidden ones.
    async fn boundary_check(
        &self,
        ctx: &ProjectCtx,
        rules: &[BoundaryRule],
    ) -> Result<Vec<BoundaryViolation>, String>;

    /// Churn × centrality ranking over files in the project.
    async fn hotspots(
        &self,
        ctx: &ProjectCtx,
        window_days: u32,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HotspotEntry>, String>;

    /// Scalar graph snapshot of the currently-pinned canonical graph.
    async fn metrics_at(
        &self,
        ctx: &ProjectCtx,
    ) -> Result<MetricsAtResult, String>;

    /// Symbols with zero incoming edges from the entry-point set
    /// (main + tests + crate-root re-exports), tiered by caller
    /// confidence.
    async fn dead_symbols(
        &self,
        ctx: &ProjectCtx,
        confidence: &str,
        limit: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String>;

    /// Scan symbols whose `documentation` or `signature` contains a
    /// `#[deprecated]` / `@deprecated` marker, and return their callers.
    async fn deprecated_callers(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
    ) -> Result<Vec<DeprecatedHit>, String>;

    /// Given entry-point and sink keys (plus queried symbols), return
    /// which queried symbols sit on any shortest path from any entry
    /// to any sink.
    async fn touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        seed_entries: &[String],
        seed_sinks: &[String],
        symbols: &[String],
    ) -> Result<Vec<HotPathHit>, String>;

    /// Files most frequently co-edited with `file_path`, derived from
    /// the commit-based coupling index (see
    /// `djinn_graph::coupling_index`). Does not consult the SCIP graph.
    async fn coupling(
        &self,
        ctx: &ProjectCtx,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CouplingEntry>, String>;

    /// Top files by distinct-commit count over the optional window,
    /// pulling from the coupling index. `since_days` maps to a UTC
    /// lower bound on `committed_at`; omit for all-time churn.
    async fn churn(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
    ) -> Result<Vec<ChurnEntry>, String>;

    /// Top file *pairs* by co-edit count, project-wide. `since_days`
    /// and `max_files_per_commit` mirror the coupling-index knobs (see
    /// `djinn_db::CommitFileChangeRepository::top_coupled_pairs`).
    async fn coupling_hotspots(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<CoupledPairEntry>, String>;

    /// Top files by cumulative coupling across all partners (sum of
    /// `co_edits` over every pair the file participates in). Useful
    /// for change-propagation risk mapping.
    async fn coupling_hubs(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<CouplingHubEntry>, String>;

    /// Pre-resolve a caller-supplied `key` (file path, SCIP symbol
    /// string, or short identifier) into either a single canonical node
    /// (`Found`), a ranked candidate list (`Ambiguous`), or a hard miss
    /// (`NotFound`). Powers the PR C2 ambiguity response — the
    /// `code_graph` dispatcher and the chat tool both call this before
    /// the heavier op so they can surface a candidate list instead of a
    /// generic `not found` error string.
    ///
    /// `kind_hint` (e.g. `"class"`, `"function"`) feeds into the score
    /// formula and lets the caller bias the disambiguation list.
    async fn resolve(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        kind_hint: Option<&str>,
    ) -> Result<ResolveOutcome, String>;
}
