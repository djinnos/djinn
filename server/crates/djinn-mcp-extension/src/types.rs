//! Shared parameter types used across extension handlers.
//!
//! These types mirror the tool-call argument shapes that the MCP surface
//! and agent reply-loop produce.  They are plain `Deserialize` structs —
//! no domain logic, no database access.

use serde::Deserialize;

#[derive(Deserialize)]
pub struct IncomingToolCall {
    pub name: String,
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
pub struct TaskListParams {
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
pub struct TaskShowParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct TaskActivityListParams {
    pub id: String,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub actor_role: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Deserialize)]
pub struct TaskUpdateParams {
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
pub struct TaskUpdateAcParams {
    pub id: String,
    pub acceptance_criteria: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct TaskCreateParams {
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
pub struct EpicShowParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct EpicUpdateParams {
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
pub struct EpicCreateParams {
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
pub struct EpicBlockersParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct ProposalShowParams {
    pub id: String,
    /// Select which top-level sections to include in the response.
    /// Accepted values: `proposal`, `targets`, `feedback`, `signoffs`,
    /// `revisions`, `debate`, `epics`, `gate_status`.
    /// Default: all fields selected. Invalid values return a validation error.
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// Controls revision body verbosity when `revisions` is selected.
    /// Accepted values: `excerpt` (default), `full`, `omit`.
    /// Ignored when `fields` omits `revisions`.
    pub revision_bodies: Option<String>,
}

#[derive(Deserialize)]
pub struct ProposalCompleteParams {
    pub id: String,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Deserialize)]
pub struct ProposalDebateAppendParams {
    pub proposal_id: String,
    pub kind: String,
    pub body: String,
    #[serde(default)]
    pub blocking: bool,
    pub agent_role: String,
    pub against_revision_seq: i32,
    pub round: i32,
}

#[derive(Deserialize)]
pub struct ProposalDebateListParams {
    pub proposal_id: String,
}

#[derive(Deserialize)]
pub struct ProposalDebateResolveParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct ProposalAcSetParams {
    pub id: String,
    /// Full acceptance-criteria list in order; entries may be bare
    /// `{"met": bool}` (criterion text is merged from the current proposal) or
    /// `{"criterion": "...", "met": bool}`.
    pub acceptance_criteria: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct ProposalAcAmendParams {
    pub id: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub amendments: Vec<ProposalAcAmendmentParams>,
}

#[derive(Deserialize)]
pub struct ProposalAcAmendmentParams {
    pub operation: String,
    pub index: usize,
    #[serde(default)]
    pub criterion: Option<String>,
}

#[derive(Deserialize)]
pub struct ProposalReconcileObsoleteEpicParams {
    pub proposal_id: String,
    pub epic_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct EpicTasksParams {
    pub id: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub struct TaskCommentAddParams {
    pub id: String,
    pub body: String,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryReadParams {
    pub identifier: String,
}

#[derive(Deserialize)]
pub struct MemorySearchParams {
    pub query: String,
    pub folder: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    pub limit: Option<i64>,
    pub task_id: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryListParams {
    pub folder: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    pub status: Option<String>,
    pub depth: Option<i64>,
}

#[derive(Deserialize)]
pub struct MemoryBuildContextParams {
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
pub struct MemoryWriteParams {
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
pub struct MemoryMoveParams {
    pub identifier: String,
    #[serde(rename = "type")]
    pub note_type: String,
    pub title: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryEditParams {
    pub identifier: String,
    pub operation: String,
    pub content: String,
    pub find_text: Option<String>,
    pub section: Option<String>,
    #[serde(rename = "type")]
    pub note_type: Option<String>,
    #[serde(default, alias = "applies_when")]
    pub retrieval_anchor: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryBrokenLinksLocalParams {
    pub folder: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryOrphansLocalParams {
    pub folder: Option<String>,
}

#[derive(Deserialize)]
pub struct ShellParams {
    pub command: String,
    pub timeout_ms: Option<u64>,
    /// Run against another registered project (UUID or owner/repo slug) instead
    /// of the task worktree. That repo is lazily checked out read-only from its
    /// bare mirror (default branch) on first use and cached for the run. Writes
    /// there are discarded — only your task project is committed.
    pub project: Option<String>,
}

#[derive(Deserialize)]
pub struct WriteParams {
    pub path: String,
    pub content: String,
}

/// Edit tool input parameters. The required schema is unchanged (`path`,
/// `old_text`, `new_text`); callers still provide `old_text` text, but the
/// matcher may rescue common whitespace, indentation, escape, boundary, and
/// Unicode drift automatically.
///
/// **Response surface:** the edit handler returns dynamic JSON. On success the
/// top-level fields `ok`, `path`, and `diagnostics` are stable and may include
/// optional `match_note` and `edit_match` objects. On failure the error string
/// embeds an `edit_match` JSON fragment. Parsers that only consume `ok`,
/// `path`, and `diagnostics` remain fully compatible.
#[derive(Deserialize)]
pub struct EditParams {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Deserialize)]
pub struct ApplyPatchParams {
    pub patch: String,
}

#[derive(Deserialize)]
pub struct ReadParams {
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
pub struct CodeSearchParams {
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
pub struct TaskTransitionParams {
    pub id: String,
    pub action: String,
    pub reason: Option<String>,
    pub target_status: Option<String>,
    /// Required when action = "force_close". UUIDs or short IDs of replacement
    /// tasks the Lead created before closing this one.
    pub replacement_task_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct TaskDeleteBranchParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct TaskArchiveActivityParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct TaskResetCountersParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct TaskKillSessionParams {
    pub id: String,
}

#[derive(Deserialize)]
pub struct LspParams {
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
pub struct CodeGraphParams {
    pub operation: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub kind_filter: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default, alias = "summaryOnly")]
    pub summary_only: Option<bool>,
    #[serde(default, alias = "byDepthCounts")]
    pub by_depth_counts: Option<bool>,
    #[serde(default, alias = "pageLimit")]
    pub page_limit: Option<usize>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub context_filter: Option<String>,
    #[serde(default)]
    pub file_filter: Option<String>,
    #[serde(default)]
    pub edge_filters: Option<Vec<String>>,
    #[serde(default)]
    pub token_budget: Option<i64>,
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
    #[serde(default)]
    pub min_confidence: Option<f64>,
    #[serde(default)]
    pub kind_hint: Option<String>,
    #[serde(default)]
    pub include_content: Option<bool>,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub rules: Option<Vec<BoundaryRule>>,
    #[serde(default)]
    pub module_glob: Option<String>,
    #[serde(default)]
    pub window_days: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub confidence: Option<String>,
    #[serde(default)]
    pub since_days: Option<u32>,
    #[serde(default)]
    pub file_glob: Option<String>,
    #[serde(default)]
    pub changed_ranges: Option<Vec<ChangedRangeArg>>,
    #[serde(default)]
    pub from_sha: Option<String>,
    #[serde(default)]
    pub to_sha: Option<String>,
    #[serde(default)]
    pub changed_files: Option<Vec<String>>,
    #[serde(default)]
    pub node_cap: Option<usize>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub tests: Option<String>,
    #[serde(default, alias = "caller_commit", alias = "currentHead")]
    pub current_head: Option<String>,
}

impl CodeGraphParams {
    /// Coerce `Some("")` to `None` on every `Option<String>` input.
    pub fn normalize(&mut self) {
        fn clear(opt: &mut Option<String>) {
            if opt.as_deref().is_some_and(str::is_empty) {
                *opt = None;
            }
        }
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

    pub fn resolved_offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }

    pub fn resolved_page_limit(&self, default: usize) -> usize {
        self.page_limit.unwrap_or(default).clamp(1, 1000)
    }

    pub fn resolved_current_head(&self) -> Option<String> {
        self.current_head
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn normalize_resolver_inputs(&mut self) {
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
#[derive(Debug, Clone, Deserialize)]
pub struct ChangedRangeArg {
    #[serde(alias = "file_path")]
    pub file: String,
    pub start_line: u32,
    #[serde(default)]
    pub end_line: Option<u32>,
}

/// One rule for the `boundary_check` op.
#[derive(Debug, Clone, Deserialize)]
pub struct BoundaryRule {
    pub name: String,
    pub from_glob: String,
    pub forbid_to: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct CiJobLogParams {
    /// Explicit Actions job id to fetch directly (escape hatch). When omitted,
    /// the failing jobs are auto-discovered from the task's recorded CI state.
    #[serde(default)]
    pub job_id: Option<u64>,
    /// Target a specific PR in the same project's repo (escalation tasks whose
    /// description names a source PR). Defaults to the task's own recorded PR.
    #[serde(default)]
    pub pr_number: Option<u64>,
    /// Optional step name to narrow the returned log to a single failed step.
    #[serde(default)]
    pub step: Option<String>,
}

#[derive(Deserialize)]
pub struct GithubSearchParams {
    pub query: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub struct GithubFetchFileParams {
    pub repo: String,
    pub path: String,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
}
