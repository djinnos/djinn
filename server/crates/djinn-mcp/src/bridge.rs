/// Bridge traits that decouple djinn-mcp from the server binary.
///
/// The server implements each trait for its concrete handle types
/// (CoordinatorHandle, SlotPoolHandle, LspManager, SyncManager, AppState).
/// McpState holds Arc<dyn Trait> so the MCP layer never imports server types.
use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct SemanticQueryEmbedding {
    pub values: Vec<f32>,
}

// ── Data types ─────────────────────────────────────────────────────────────────
// Plain data returned by the bridge traits. Defined here so they can be used
// by both djinn-mcp tool handlers and the server bridge implementations.

#[derive(Debug, Clone)]
pub struct CoordinatorStatus {
    pub paused: bool,
    pub tasks_dispatched: u64,
    pub sessions_recovered: u64,
    pub unhealthy_projects: HashMap<String, String>,
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

#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    pub name: String,
    pub branch: String,
    pub enabled: bool,
    /// Sync-enabled project paths.
    pub project_paths: Vec<String>,
    pub last_synced_at: Option<String>,
    pub last_error: Option<String>,
    pub failure_count: u32,
    pub backoff_secs: u64,
    pub needs_attention: bool,
}

#[derive(Debug, Serialize)]
pub struct SyncResult {
    pub channel: String,
    pub ok: bool,
    pub count: Option<usize>,
    pub error: Option<String>,
}

// ── Coordinator ─────────────────────────────────────────────────────────────────

#[async_trait]
pub trait CoordinatorOps: Send + Sync {
    async fn resume_project(&self, project_id: &str) -> Result<(), String>;
    async fn resume(&self) -> Result<(), String>;
    async fn pause_project(&self, project_id: &str) -> Result<(), String>;
    async fn pause_project_immediate(&self, project_id: &str, reason: &str) -> Result<(), String>;
    async fn pause_immediate(&self, reason: &str) -> Result<(), String>;
    fn get_status(&self) -> Result<CoordinatorStatus, String>;
    fn get_project_status(&self, project_id: &str) -> Result<CoordinatorStatus, String>;
    async fn validate_project_health(&self, project_id: Option<String>) -> Result<(), String>;
    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String>;
    async fn pause(&self) -> Result<(), String>;
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

// ── Sync ────────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SyncOps: Send + Sync {
    async fn enable_project(&self, project_id: &str) -> Result<(), String>;
    async fn disable_project(&self, project_id: &str) -> Result<(), String>;
    async fn delete_remote_branch(&self, channel: &str, project_path: &Path) -> Result<(), String>;
    async fn export_all(&self, user_id: Option<&str>) -> Vec<SyncResult>;
    async fn import_all(&self) -> Vec<SyncResult>;
    async fn status(&self) -> Vec<ChannelStatus>;
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
// building the graph from SCIP artifacts; djinn-mcp/djinn-agent never depend
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

/// A search hit from the name-index lookup. Returned by `search`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchHit {
    pub key: String,
    pub kind: String,
    pub display_name: String,
    pub score: f64,
    pub file: Option<String>,
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

/// A node referenced by a graph diff.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphDiffNode {
    pub key: String,
    pub kind: String,
    pub display_name: String,
}

/// An edge referenced by a graph diff.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphDiffEdge {
    pub from: String,
    pub to: String,
    pub edge_kind: String,
}

/// Result of a `status` query — a peek at the in-memory canonical graph cache
/// for a project.  No warming side effects.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphStatus {
    pub project_id: String,
    pub warmed: bool,
    pub last_warm_at: Option<String>,
    pub pinned_commit: Option<String>,
    pub commits_since_pin: Option<u64>,
}

/// Result of a `diff` query — what changed between two graph snapshots.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GraphDiff {
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub added_nodes: Vec<GraphDiffNode>,
    pub removed_nodes: Vec<GraphDiffNode>,
    pub added_edges: Vec<GraphDiffEdge>,
    pub removed_edges: Vec<GraphDiffEdge>,
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
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ImpactEntry {
    pub key: String,
    pub depth: usize,
}

/// Either symbol-level neighbors/impact or per-file rollup.
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

#[async_trait]
pub trait RepoGraphOps: Send + Sync {
    /// Neighbors of a file or symbol node (edges in/out). When `group_by` is
    /// `Some("file")`, results are collapsed into per-file rollups.
    async fn neighbors(
        &self,
        project_path: &str,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
    ) -> Result<NeighborsResult, String>;

    /// Top-ranked nodes by PageRank + structural weight. `sort_by` can be one
    /// of `pagerank` (default), `in_degree`, `out_degree`, or `total_degree`.
    async fn ranked(
        &self,
        project_path: &str,
        kind_filter: Option<&str>,
        sort_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedNode>, String>;

    /// Symbols that implement a given trait/interface symbol.
    async fn implementations(
        &self,
        project_path: &str,
        symbol: &str,
    ) -> Result<Vec<String>, String>;

    /// Transitive impact set — nodes that depend on the queried node. When
    /// `group_by` is `Some("file")`, results are collapsed into per-file
    /// rollups.
    async fn impact(
        &self,
        project_path: &str,
        key: &str,
        depth: usize,
        group_by: Option<&str>,
    ) -> Result<ImpactResult, String>;

    /// Name-based symbol search.
    async fn search(
        &self,
        project_path: &str,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String>;

    /// Strongly-connected components of size >= `min_size`.
    async fn cycles(
        &self,
        project_path: &str,
        kind_filter: Option<&str>,
        min_size: usize,
    ) -> Result<Vec<CycleGroup>, String>;

    /// Bulk dead-symbol enumeration (nodes with zero incoming references).
    async fn orphans(
        &self,
        project_path: &str,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String>;

    /// Shortest dependency path between two nodes.
    async fn path(
        &self,
        project_path: &str,
        from: &str,
        to: &str,
        max_depth: Option<usize>,
    ) -> Result<Option<PathResult>, String>;

    /// Enumerate edges matching path globs.
    async fn edges(
        &self,
        project_path: &str,
        from_glob: &str,
        to_glob: &str,
        edge_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EdgeEntry>, String>;

    /// Diff between the current canonical graph and its in-memory predecessor.
    /// `since` accepts `None` or `Some("previous")`. Anything else returns an
    /// error explaining persistent diff is not yet supported.
    async fn diff(
        &self,
        project_path: &str,
        since: Option<&str>,
    ) -> Result<Option<GraphDiff>, String>;

    /// Detailed description of a single symbol.
    async fn describe(
        &self,
        project_path: &str,
        key: &str,
    ) -> Result<Option<SymbolDescription>, String>;

    /// Peek at the in-memory canonical graph cache for the given project.
    /// MUST NOT trigger any warming or SCIP indexing.  When the cache is
    /// empty for this project, returns `warmed: false` with the timestamp/
    /// commit fields set to `None`.
    async fn status(&self, project_path: &str) -> Result<GraphStatus, String>;
}
