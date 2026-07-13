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
    /// Epic dependencies (UUIDs or short_ids; may be cross-project) that must
    /// close before this epic's breakdown auto-dispatches.
    pub blocked_by_add: Option<Vec<String>>,
    pub blocked_by_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct EpicCreateParams {
    pub title: String,
    pub description: Option<String>,
    pub memory_refs: Option<Vec<String>>,
    pub auto_breakdown: Option<bool>,
    /// Other registered projects (UUIDs or owner/repo slugs) this epic's tasks
    /// may READ while writing only to its own project.
    pub read_sources: Option<Vec<String>>,
    /// Proposal (UUID or short_id) this epic is being decomposed from — records
    /// the proposal → epic link (Planner Mode D).
    pub proposal_id: Option<String>,
    /// Target project for the epic (UUID or owner/repo slug). When omitted the
    /// epic is created on the session's resolved project. Mode D sets this per
    /// target so a single breakdown run can create epics across repos.
    pub project: Option<String>,
    /// Epics (UUIDs or short_ids; may be cross-project) that must close before
    /// this epic's breakdown auto-dispatches. Mode D uses this to sequence the
    /// epics it creates (e.g. a consumer epic blocked on a schema epic).
    pub blocked_by: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct EpicBlockersParams {
    pub id: String,
}

#[derive(Deserialize)]
pub(super) struct ProposalShowParams {
    pub id: String,
    /// Select which top-level sections to include in the response.
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// Controls revision body verbosity: `excerpt` (default), `full`, `omit`.
    pub revision_bodies: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ProposalCompleteParams {
    pub id: String,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ProposalAcSetParams {
    pub id: String,
    /// Full acceptance-criteria list in order; entries may be bare
    /// `{"met": bool}` (criterion text is merged from the current proposal) or
    /// `{"criterion": "...", "met": bool}`.
    pub acceptance_criteria: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub(super) struct ProposalAcAmendParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub amendments: Vec<ProposalAcAmendmentParams>,
}

#[derive(Deserialize)]
pub(super) struct ProposalAcAmendmentParams {
    pub operation: String,
    pub index: usize,
    #[serde(default)]
    pub criterion: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ProposalReconcileObsoleteEpicParams {
    pub proposal_id: String,
    pub epic_id: String,
    #[serde(default)]
    pub reason: Option<String>,
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
    pub status: Option<String>,
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
    pub reason: String,
    #[serde(rename = "type")]
    pub note_type: String,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    pub scope_paths: Option<Vec<String>>,
    #[serde(default, alias = "applies_when")]
    pub retrieval_anchor: Option<String>,
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
    pub reason: String,
    pub find_text: Option<String>,
    pub section: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    #[serde(default, alias = "applies_when")]
    pub retrieval_anchor: Option<String>,
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
pub(super) struct ShellParams {
    pub command: String,
    pub timeout_ms: Option<u64>,
    /// Run against another registered project (UUID or owner/repo slug) instead
    /// of the task worktree. That repo is lazily checked out read-only from its
    /// bare mirror (default branch) on first use and cached for the run. Writes
    /// there are discarded — only your task project is committed.
    pub project: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct WriteParams {
    pub path: String,
    pub content: String,
}

/// Edit tool input parameters. Fields are unchanged from the original schema;
/// callers still provide exact `old_text`, but the matcher may rescue common
/// whitespace, indentation, escape, boundary, and Unicode drift automatically.
///
/// **Response surface (dynamic JSON):** success returns `{ok, path, diagnostics}`
/// plus optional `match_note` and `edit_match` (strategy, byte/line ranges,
/// reindented, unicode_splice, note). Failure returns an error string containing
/// an `edit_match` JSON fragment with strategy and outcome-specific fields
/// (candidate_count for ambiguous; nearest_miss for no_match; guard_reason for
/// guard_rejected). Existing callers that only parse `ok`, `path`, and
/// `diagnostics` remain fully compatible.
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
    /// Read from another registered project (UUID or owner/repo slug) instead
    /// of the task worktree. Served read-only from that repo's bare mirror at
    /// its default branch — no working clone.
    pub project: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CodeSearchParams {
    /// Search pattern (basic regex, like `git grep`).
    pub query: String,
    /// Limit to one registered project (UUID or owner/repo slug). Omit (or
    /// pass "*") to search ALL registered projects.
    pub project: Option<String>,
    /// Optional pathspec to scope the search (e.g. `crates/` or `*.rs`).
    pub path: Option<String>,
    pub ignore_case: Option<bool>,
    /// Max hits per project (default 100).
    pub max_results: Option<usize>,
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
    /// Project slug/UUID accepted by the public schema. The agent-side chat
    /// dispatcher resolves the project before this struct is used, so this is
    /// retained only to mirror the MCP request surface and tolerate callers
    /// that include it.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    /// Stable graph node UID returned by prior `code_graph` calls. Exact-match
    /// follow-up input; normalized into `key` before bridge dispatch.
    #[serde(default)]
    pub uid: Option<String>,
    /// Human-readable resolver input. Use with `file_path` and `kind` when a
    /// prior `uid` is unavailable.
    #[serde(default)]
    pub name: Option<String>,
    /// Resolver kind hint paired with `name` + `file_path`.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub kind_filter: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Page offset for traversal triage responses (`neighbors`, `impact`,
    /// `coupling_hotspots`). Pagination is an agent-boundary view only.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Counts-only traversal triage mode. CamelCase is accepted from MCP/chat
    /// clients and normalized by serde.
    #[serde(default, alias = "summaryOnly")]
    pub summary_only: Option<bool>,
    /// Request per-depth impact counts. CamelCase is accepted from MCP/chat.
    #[serde(default, alias = "byDepthCounts")]
    pub by_depth_counts: Option<bool>,
    /// Distinct page cap for `impact`, where `limit` remains traversal depth.
    #[serde(default, alias = "pageLimit")]
    pub page_limit: Option<usize>,
    #[serde(default)]
    pub query: Option<String>,
    /// Natural-language subgraph query narrowing: coarse subsystem/API/type
    /// text. Mirrors control-plane `CodeGraphParams::context_filter` for
    /// `operation: "query_subgraph"`.
    #[serde(default)]
    pub context_filter: Option<String>,
    /// Natural-language subgraph query path/file substring filter. Mirrors
    /// control-plane `file_filter`; `file_glob` remains a compatibility alias.
    #[serde(default)]
    pub file_filter: Option<String>,
    /// Explicit edge kinds for `query_subgraph` traversal. Omit to let the
    /// planner infer useful kinds from the question; `edge_kind` remains a
    /// single-kind compatibility alias.
    #[serde(default)]
    pub edge_filters: Option<Vec<String>>,
    /// Approximate response token budget for `query_subgraph`; positive values
    /// are clamped at dispatch, zero/negative values are rejected.
    #[serde(default)]
    pub token_budget: Option<i64>,
    /// Maximum seed count for `query_subgraph`; positive values are clamped at
    /// dispatch, zero/negative values are rejected.
    #[serde(default)]
    pub max_seeds: Option<i64>,
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
    /// Semantic zoom level for `snapshot`: `symbol` keeps the existing
    /// file/symbol-node payload shape; `community` is accepted for forward
    /// compatibility with the collapsed community view.
    #[serde(default)]
    pub level: Option<String>,
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
    /// Optional module-path glob for `api_surface` (filter symbols by file).
    #[serde(default)]
    pub module_glob: Option<String>,
    /// Churn look-back window in days for `hotspots`; MCP alias for
    /// agent-side `since_days`.
    #[serde(default)]
    pub window_days: Option<u32>,
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
    /// Iter 28 `complexity` op: target tier — `"functions"` (default)
    /// or `"files"`. Reuses the existing `sort_by`, `file_glob`, and
    /// `limit` fields for the metric / glob / cap.
    #[serde(default)]
    pub target: Option<String>,
    /// v10: test-file filter. Honoured by the MCP surface for `snapshot`; kept
    /// here so agent schema/params remain additive-compatible.
    #[serde(default)]
    pub tests: Option<String>,
    /// jc47: caller's current HEAD / git commit SHA. When supplied, every
    /// successful `code_graph` response includes an additive
    /// `graph_staleness` object comparing this commit against the cached
    /// graph blob's pinned commit (only populated when the chat
    /// dispatcher's `current_head` is non-empty; absent otherwise so
    /// existing callers retain their previous response shape). Empty /
    /// whitespace values are normalized to `None` so chat-side LLMs that
    /// emit every field as `""` don't accidentally trigger staleness
    /// computation. `caller_commit` and `currentHead` are accepted as
    /// aliases. Behavior is serve-stale-with-flag only: never blocks
    /// query execution, never auto-triggers graph re-warming.
    #[serde(default, alias = "caller_commit", alias = "currentHead")]
    pub current_head: Option<String>,
}

impl CodeGraphParams {
    /// Coerce `Some("")` to `None` on every `Option<String>` input.
    ///
    /// Why: chat-side LLMs frequently emit tool calls with every schema
    /// field present, defaulting unset optionals to `""`. Downstream
    /// handlers that build a glob from `Some("")` get an "empty pattern"
    /// matcher that matches nothing, silently filtering all results.
    /// Normalizing at the param boundary turns these into `None`, so
    /// every op sees the same shape regardless of caller behavior.
    pub(super) fn normalize(&mut self) {
        fn clear(opt: &mut Option<String>) {
            if opt.as_deref().is_some_and(str::is_empty) {
                *opt = None;
            }
        }
        // jc47: treat whitespace-only `current_head` the same as empty —
        // chat-side LLMs occasionally emit `"  "` instead of `""` for unset
        // fields, and we don't want a whitespace string to slip through and
        // silently trigger staleness attachment against a meaningless caller
        // commit. Trimmed-empty matches the contract documented on the field
        // ("Empty / whitespace values are normalized to `None`").
        fn clear_trimmed(opt: &mut Option<String>) {
            if opt.as_deref().is_some_and(|s| s.trim().is_empty()) {
                *opt = None;
            }
        }
        clear(&mut self.key);
        clear(&mut self.uid);
        clear(&mut self.name);
        clear(&mut self.kind);
        clear(&mut self.workspace);
        clear(&mut self.direction);
        clear(&mut self.kind_filter);
        clear(&mut self.query);
        clear(&mut self.project);
        clear(&mut self.context_filter);
        clear(&mut self.file_filter);
        clear(&mut self.from);
        clear(&mut self.to);
        clear(&mut self.from_glob);
        clear(&mut self.to_glob);
        clear(&mut self.visibility);
        clear(&mut self.sort_by);
        clear(&mut self.group_by);
        clear(&mut self.edge_kind);
        clear(&mut self.kind_hint);
        clear(&mut self.mode);
        clear(&mut self.file_path);
        clear(&mut self.confidence);
        clear(&mut self.file_glob);
        clear(&mut self.module_glob);
        clear(&mut self.from_sha);
        clear(&mut self.to_sha);
        clear(&mut self.level);
        clear(&mut self.target);
        clear(&mut self.tests);
        clear_trimmed(&mut self.current_head);
    }

    pub(super) fn resolved_offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }

    pub(super) fn resolved_page_limit(&self, default: usize) -> usize {
        self.page_limit.unwrap_or(default).clamp(1, 1000)
    }

    /// jc47: caller-supplied `current_head` trimmed to a real value.
    /// Returns `None` when the field is missing or only whitespace, so
    /// the chat dispatcher's staleness attachment can short-circuit
    /// cleanly without re-validating at every op site.
    pub(super) fn resolved_current_head(&self) -> Option<String> {
        self.current_head
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Normalize public resolver aliases to the legacy `key` + `kind_hint`
    /// fields consumed by the bridge. `uid` wins as an exact identity; `name`
    /// is used only when neither `uid` nor `key` was supplied. `kind` mirrors
    /// the MCP triplet vocabulary and feeds the existing disambiguation score.
    pub(super) fn normalize_resolver_inputs(&mut self) {
        if let Some(uid) = self.uid.as_deref().filter(|s| !s.is_empty()) {
            self.key = Some(uid.to_string());
        } else if self.key.as_deref().filter(|s| !s.is_empty()).is_none()
            && let Some(name) = self.name.as_deref().filter(|s| !s.is_empty())
        {
            self.key = Some(name.to_string());
        }

        if self
            .kind_hint
            .as_deref()
            .filter(|s| !s.is_empty())
            .is_none()
            && let Some(kind) = self.kind.as_deref().filter(|s| !s.is_empty())
        {
            self.kind_hint = Some(kind.to_string());
        }
    }
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
    /// Optional human-readable explanation of why this rule exists,
    /// surfaced in CI output so violations are self-documenting.
    #[serde(default)]
    pub description: Option<String>,
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
