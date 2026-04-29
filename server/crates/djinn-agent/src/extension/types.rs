use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct IncomingToolCall {
    pub name: String,
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
pub(super) struct TaskListParams {
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    #[serde(alias = "q")]
    pub text: Option<String>,
    pub label: Option<String>,
    pub parent: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskShowParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskActivityListParams {
    pub id: String,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub actor_role: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskUpdateParams {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub labels_add: Option<Vec<String>>,
    pub labels_remove: Option<Vec<String>>,
    pub acceptance_criteria: Option<Vec<serde_json::Value>>,
    pub memory_refs_add: Option<Vec<String>>,
    pub memory_refs_remove: Option<Vec<String>>,
    #[serde(default)]
    pub blocked_by_add: Vec<String>,
    #[serde(default)]
    pub blocked_by_remove: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct TaskUpdateAcParams {
    pub id: String,
    pub acceptance_criteria: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub(super) struct TaskCreateParams {
    pub epic_id: String,
    pub title: String,
    pub issue_type: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub status: Option<String>,
    pub acceptance_criteria: Option<Vec<serde_json::Value>>,
    pub blocked_by: Option<Vec<String>>,
    pub memory_refs: Option<Vec<String>>,
    /// Specialist role name to route this task (e.g. "rust-expert").
    pub agent_type: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct EpicShowParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct EpicUpdateParams {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub memory_refs_add: Option<Vec<String>>,
    pub memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct EpicTasksParams {
    pub id: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct TaskCommentAddParams {
    pub id: String,
    pub body: String,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryReadParams {
    pub identifier: String,
}

#[derive(Deserialize)]
pub(super) struct MemorySearchParams {
    pub query: String,
    pub folder: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    pub limit: Option<i64>,
    pub task_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryListParams {
    pub folder: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    pub depth: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct MemoryBuildContextParams {
    pub url: Option<String>,
    /// Link traversal depth (default 1). Currently unused at the dispatch layer.
    pub _depth: Option<i64>,
    pub max_related: Option<i64>,
    pub budget: Option<i64>,
    pub task_id: Option<String>,
    pub _query: Option<String>,
    pub limit: Option<i64>,
    pub min_confidence: Option<f64>,
}

#[derive(Deserialize)]
pub(super) struct MemoryWriteParams {
    pub title: String,
    pub content: String,
    #[serde(rename = "type")]
    pub note_type: String,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    pub scope_paths: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct MemoryMoveParams {
    pub identifier: String,
    #[serde(rename = "type")]
    pub note_type: String,
    pub title: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryEditParams {
    pub identifier: String,
    pub operation: String,
    pub content: String,
    pub find_text: Option<String>,
    pub section: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryBrokenLinksLocalParams {
    pub folder: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct MemoryOrphansLocalParams {
    pub folder: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AgentAmendPromptParams {
    pub agent_id: String,
    pub amendment: String,
    pub metrics_snapshot: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ShellParams {
    pub command: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct WriteParams {
    pub path: String,
    pub content: String,
}

#[derive(Deserialize)]
pub(super) struct EditParams {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Deserialize)]
pub(super) struct ApplyPatchParams {
    pub patch: String,
}

#[derive(Deserialize)]
pub(super) struct ReadParams {
    #[serde(alias = "path")]
    pub file_path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

// ── Lead-only tool params ───────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct TaskTransitionParams {
    pub id: String,
    pub action: String,
    pub reason: Option<String>,
    pub target_status: Option<String>,
    /// Required when action = "force_close". UUIDs or short IDs of replacement
    /// tasks the Lead created before closing this one.
    pub replacement_task_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct TaskDeleteBranchParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskArchiveActivityParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskResetCountersParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct TaskKillSessionParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct LspParams {
    pub operation: String,
    pub file_path: String,
    pub line: Option<u32>,
    pub character: Option<u32>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name_filter: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CodeGraphParams {
    pub operation: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub kind_filter: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub from_glob: Option<String>,
    #[serde(default)]
    pub to_glob: Option<String>,
    #[serde(default)]
    pub min_size: Option<usize>,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub sort_by: Option<String>,
    #[serde(default)]
    pub group_by: Option<String>,
    #[serde(default)]
    pub max_depth: Option<usize>,
    #[serde(default)]
    pub edge_kind: Option<String>,
    /// Minimum edge confidence in `[0, 1]` for the `impact` BFS frontier
    /// (PR A2). Mirrors the MCP-side
    /// `djinn_control_plane::tools::graph_tools::CodeGraphParams::min_confidence`
    /// — see there for the full doc string.
    #[serde(default)]
    pub min_confidence: Option<f64>,
    /// PR C2: optional kind hint biasing the disambiguation score when
    /// `key` is a short identifier and the resolver hits multiple
    /// candidates. Accepts the same labels the resolver emits:
    /// `"file"`, `"class"`, `"function"`, `"method"`, etc.
    #[serde(default)]
    pub kind_hint: Option<String>,
    /// PR C1: when `true`, the `context` op populates the symbol body
    /// inline. Default `false` (bandwidth gate).
    #[serde(default)]
    pub include_content: Option<bool>,
    /// PR B4: search mode for the `search` op. `"name"` (legacy fast
    /// path) or `"hybrid"` (RRF over lexical + semantic + structural).
    /// When omitted the dispatcher falls back to
    /// `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` (default `"name"`).
    #[serde(default)]
    pub mode: Option<String>,
    /// v8 `boundary_check` op: list of architectural rules to enforce.
    /// Each rule names a `from_glob` and a list of `forbid_to` globs;
    /// any edge whose source file matches the from-glob and whose
    /// target file matches any forbid-to-glob is reported as a
    /// violation. Empty / absent for every other op.
    #[serde(default)]
    pub rules: Option<Vec<BoundaryRule>>,
    /// v8 `symbols_at` op: 1-indexed end line for the range query.
    /// Both `start_line` and `end_line` are now first-class fields
    /// (the old `min_size` overload remains as a fallback).
    #[serde(default)]
    pub end_line: Option<u32>,
    /// v8 `symbols_at` op: 1-indexed start line for the range query.
    /// First-class field — see `end_line` doc.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// v8 `symbols_at` / `dead_symbols` / etc: explicit `file_path`
    /// alias for the queried file, instead of overloading `key` with
    /// the `file:` prefix. When both are set, `file_path` wins.
    #[serde(default)]
    pub file_path: Option<String>,
    /// v8 `dead_symbols` op: confidence band for the dead-code
    /// detector ("high" | "med" | "low"). Distinct from `kind_filter`
    /// (which restricts node kinds for ranked/orphans/cycles).
    #[serde(default)]
    pub confidence: Option<String>,
    /// v8 `hotspots` / `churn` / `coupling_hubs` ops: look-back
    /// window in days. First-class field — the previous overload
    /// of `query` to carry an integer is kept as a fallback.
    #[serde(default)]
    pub since_days: Option<u32>,
    /// v8 `hotspots` op: file path glob to narrow the hotspot set.
    /// Distinct from `from_glob` so a caller can pass both an
    /// architectural and a path-shape filter.
    #[serde(default)]
    pub file_glob: Option<String>,
    /// v8 `diff_touches` op: list of changed file/line-range
    /// records parsed from `git diff --unified=0 base..head`.
    /// Each entry passes through to the bridge as-is.
    #[serde(default)]
    pub changed_ranges: Option<Vec<ChangedRangeArg>>,
    /// v8 `detect_changes` op: SHA range or explicit changed-files
    /// list. When `from_sha` is set, the bridge shells out to git
    /// diff; when `changed_files` is set, those files are taken as
    /// the touched set wholesale.
    #[serde(default)]
    pub from_sha: Option<String>,
    #[serde(default)]
    pub to_sha: Option<String>,
    #[serde(default)]
    pub changed_files: Option<Vec<String>>,
    /// v8 `snapshot` op: cap on returned node count. Default 2000
    /// (Sigma WebGL ceiling); the trait clamps to 10k.
    #[serde(default)]
    pub node_cap: Option<usize>,
}

/// v8 `diff_touches` input shape — mirrors
/// `djinn_control_plane::bridge::ChangedRange`.
///
/// Accepts EITHER `file` (matches the bridge ChangedRange + the
/// MCP-server-advertised schema) OR `file_path` (legacy / agent-dispatch
/// alias) so both client conventions deserialize correctly. End_line
/// defaults to start_line for single-line hunks (matches bridge default).
#[derive(Debug, Clone, Deserialize)]
pub(super) struct ChangedRangeArg {
    #[serde(alias = "file_path")]
    pub file: String,
    pub start_line: u32,
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// One rule for the `boundary_check` op. Names an architectural
/// invariant the reviewer wants enforced — e.g. "domain must not
/// depend on transport". The op walks `graph_ops.edges(from_glob,
/// forbid_to_glob)` for each `forbid_to` entry and reports any
/// matching edges.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct BoundaryRule {
    /// Human-readable rule label, surfaced in violations so the
    /// reviewer can map a hit back to the policy that flagged it.
    pub name: String,
    /// Glob matched against the source file of each edge.
    pub from_glob: String,
    /// Globs matched against the target file of each edge. ANY match
    /// is a violation. Allows expressing "domain must not depend on
    /// any of {transport, api, http_handlers}" without splitting it
    /// into separate rules.
    pub forbid_to: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct CiJobLogParams {
    pub job_id: u64,
    pub step: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GithubSearchParams {
    pub query: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GithubFetchFileParams {
    pub repo: String,
    pub path: String,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
}
