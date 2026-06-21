use serde::{Deserialize, Serialize};

use super::*;

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    /// Absolute path to the project directory.
    pub project: String,
    pub title: String,
    pub content: String,
    /// Note type: adr, pattern, case, pitfall, research, requirement,
    /// reference, design, session, persona, journey, design_spec,
    /// competitive, tech_spike, brief (singleton), roadmap (singleton).
    #[serde(rename = "type")]
    #[schemars(rename = "type")]
    pub note_type: String,
    /// Optional explicit status for routed note types. For ADRs, `proposed`
    /// writes into `.djinn/decisions/proposed/`.
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    /// Crate/module path prefixes this note applies to. Empty array means global.
    pub scope_paths: Option<Vec<String>>,
    /// Objective situation where this note should be retrieved. Also accepted as
    /// `applies_when` for prompt/API language compatibility.
    #[serde(default, alias = "applies_when")]
    pub retrieval_anchor: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    pub project: String,
    /// Note permalink (e.g. "decisions/my-adr") or title.
    pub identifier: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MemoryConfirmParams {
    pub project: String,
    /// Note permalink or note ID.
    pub identifier: String,
    /// Optional reason for confirmation.
    pub comment: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EditParams {
    pub project: String,
    /// Note permalink or title.
    pub identifier: String,
    /// Operation: "append", "prepend", "find_replace", "replace_section".
    pub operation: String,
    pub content: String,
    /// Required for find_replace: exact text to search for.
    pub find_text: Option<String>,
    /// Required for replace_section: heading text identifying the section.
    pub section: Option<String>,
    /// If provided and different from current type, move the note to the new
    /// type's folder. Allowed values same as memory_write type.
    #[serde(rename = "type")]
    #[schemars(rename = "type")]
    pub note_type: Option<String>,
    /// Replace the note's retrieval anchor without modifying the note body.
    /// Also accepted as `applies_when`.
    #[serde(default, alias = "applies_when")]
    pub retrieval_anchor: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    pub project: String,
    pub query: String,
    pub folder: Option<String>,
    #[serde(rename = "type")]
    #[schemars(rename = "type")]
    pub note_type: Option<String>,
    pub limit: Option<i64>,
    /// Optional list of edge kinds to include in graph traversal scoring.
    /// When provided, only edges whose `kind` matches one of these values
    /// participate in spreading activation. Omit to use all edge kinds.
    pub edge_kinds: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListParams {
    pub project: String,
    /// Filter by folder. Omit to list all notes.
    pub folder: Option<String>,
    /// Filter by note type (e.g. "adr", "reference", "research").
    #[serde(rename = "type")]
    #[schemars(rename = "type")]
    pub note_type: Option<String>,
    /// Explicit lifecycle status filter. Defaults to active; use archived or
    /// deprecated to list non-live notes.
    pub status: Option<String>,
    /// Depth control: 0 = unlimited, 1 = exact folder (default), N = N levels.
    pub depth: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeleteParams {
    pub project: String,
    pub identifier: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MoveParams {
    pub project: String,
    pub identifier: String,
    /// New note type to move the note to. Use `proposed_adr` to recover a
    /// mis-routed ADR draft into `.djinn/decisions/proposed/`.
    #[serde(rename = "type")]
    #[schemars(rename = "type")]
    pub note_type: String,
    /// Optional new title; keep current title if omitted.
    pub title: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HealthParams {
    /// Absolute path to the project directory.
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExtractedAuditParams {
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CatalogParams {
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecentParams {
    pub project: String,
    /// Timeframe string, e.g. "7d", "24h", "today", "last week". Default: "7d".
    pub timeframe: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    pub project: String,
    pub permalink: String,
    pub limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DiffParams {
    pub project: String,
    pub permalink: String,
    /// Specific commit SHA. Omit to get the diff for the most recent change.
    pub sha: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BuildContextParams {
    pub project: String,
    /// Memory URI: "memory://folder/note", "folder/note", or "folder/*" for all
    /// notes in a folder.
    pub url: String,
    /// Link traversal depth (default 1).
    pub depth: Option<i64>,
    /// Maximum related notes to return (default 10).
    pub max_related: Option<i64>,
    /// Token budget for context (default 4096). Seed notes are uncapped.
    #[schemars(with = "i64")]
    pub budget: Option<i64>,
    /// Optional task ID for task-affinity scoring in RRF retrieval.
    pub task_id: Option<String>,
    /// Minimum confidence threshold for related notes (default 0.1). Notes
    /// below this threshold are excluded from the context response. Set to 0.0
    /// to include all notes regardless of confidence.
    pub min_confidence: Option<f64>,
    /// Optional list of edge kinds to include in graph traversal scoring.
    /// When provided, only edges whose `kind` matches one of these values
    /// participate in spreading activation. Omit to use all edge kinds.
    pub edge_kinds: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskRefsParams {
    pub project: String,
    pub permalink: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GraphParams {
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BrokenLinksParams {
    pub project: String,
    pub folder: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct OrphansParams {
    pub project: String,
    pub folder: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RepairEmbeddingsParams {
    /// Project path, slug, or id (same forms accepted by other memory tools).
    pub project: String,
    /// If true, re-embed every note even if its content hash already matches.
    /// Default false: only notes with missing or stale embeddings are repaired.
    pub force: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RunEnrichmentParams {
    /// Project path, slug, or id (same forms accepted by other memory tools).
    pub project: String,
    /// When true, schedule enrichment on a background tokio task and return
    /// immediately with `status="queued"`. When false (the default), run the
    /// pass synchronously and embed the structured report in the response.
    ///
    /// Background execution mirrors the diei roadmap's "best-effort, never
    /// blocks retrieval" constraint: the spawn path yields to the runtime so
    /// `memory_graph` (and the wider MCP surface) keep serving while the
    /// pass runs.
    pub background: Option<bool>,
}

#[derive(Serialize, schemars::JsonSchema, Default)]
pub struct MemoryRepairEmbeddingFailure {
    pub note_id: String,
    pub reason: String,
}

#[derive(Serialize, schemars::JsonSchema, Default)]
pub struct MemoryRepairEmbeddingsResponse {
    // Counters are `i64` rather than `usize` so the generated MCP JSON
    // schema lands on `format: int64` instead of the nonstandard `uint`
    // pinned by `tool_schemas::mcp_tools_list_schemas_do_not_use_nonstandard_uint_…`.
    /// Total notes scanned for the project.
    pub total: i64,
    /// Notes that received a fresh embedding during this run.
    pub repaired: i64,
    /// Notes whose embedding meta was already current; no work performed.
    pub up_to_date: i64,
    /// Notes whose re-embed attempt failed (Qdrant down, model error, …).
    pub failed: i64,
    /// First-N failures (capped to keep the response small).
    pub failures: Vec<MemoryRepairEmbeddingFailure>,
    /// Top-level failure: project not found, embedding provider unavailable, etc.
    pub error: Option<String>,
    /// Total code chunks scanned for the project. Always `0` until the
    /// chunker (PR B2) and embedding pipeline (PR B3) ship.
    pub code_chunks_total: i64,
    /// Code chunks that received a fresh embedding during this run.
    pub code_chunks_repaired: i64,
    /// Code chunks whose embedding meta was already current.
    pub code_chunks_up_to_date: i64,
    /// Code chunks whose re-embed attempt failed.
    pub code_chunks_failed: i64,
}

/// MCP response for `memory_run_enrichment`. The `report` is present when
/// `status="completed"` (foreground execution). For
/// `status="queued"` (background execution) the report is `None`; the pass
/// emits the structured report through its own `INFO` log line at finish.
#[derive(Serialize, schemars::JsonSchema, Default)]
pub struct MemoryRunEnrichmentResponse {
    /// Status of the trigger call — see [`crate::bridge::EnrichmentStatus`].
    pub status: String,
    /// Resolved DB project id (slug → id translation is performed by the
    /// tool's project resolver, same as every other memory tool).
    pub project_id: Option<String>,
    /// Structured enrichment report. Present only when
    /// `status="completed"`. None when queued, and None when the trigger
    /// fails (in which case `error` is populated).
    pub report: Option<crate::bridge::EnrichmentReport>,
    /// Top-level failure: project not found, enrichment subsystem not
    /// configured, provider errors that propagate before the pass begins.
    /// Per-batch provider failures land in `report.warnings` rather than
    /// here — the pass is best-effort and never blocks retrieval.
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AssociationsParams {
    pub project: String,
    /// Note ID or permalink (e.g. "decisions/my-adr").
    pub identifier: String,
    /// Minimum association weight [0.0, 1.0]. Default: 0.0 (all associations).
    pub min_weight: Option<f64>,
    /// Maximum number of results. Default: 20.
    pub limit: Option<i64>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryAssociationEntry {
    pub note_permalink: String,
    pub note_title: String,
    pub weight: f64,
    /// Association edge kind as stored on `note_associations.kind` (e.g. `"co_access"`).
    /// Today the only value written is `"co_access"`; future wave-1 graph-typed edges
    /// (builds_on / contradicts / supersedes / exemplifies) will widen the value set
    /// — see Epic 2chl. `#[serde(default)]` keeps older clients deserialising cleanly
    /// until they adopt the new field.
    #[serde(default)]
    pub kind: String,
    pub co_access_count: i64,
    pub last_co_access: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryAssociationsResponse {
    pub associations: Vec<MemoryAssociationEntry>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryExtractedAuditResponse {
    pub scanned_note_count: Option<i64>,
    pub merge_candidates: Option<Vec<djinn_memory::ExtractedNoteAuditFinding>>,
    pub underspecified: Option<Vec<djinn_memory::ExtractedNoteAuditFinding>>,
    pub demote_to_working_spec: Option<Vec<djinn_memory::ExtractedNoteAuditFinding>>,
    pub archive_candidates: Option<Vec<djinn_memory::ExtractedNoteAuditFinding>>,
    pub rerun_hint: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResolvedMention {
    pub short_id: String,
    pub entity_type: String,
    pub title: String,
    pub permalink: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryNoteResponse {
    pub id: Option<String>,
    pub project_id: Option<String>,
    pub permalink: Option<String>,
    pub title: Option<String>,
    pub file_path: Option<String>,
    pub note_type: Option<String>,
    pub folder: Option<String>,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    pub content: Option<String>,
    pub retrieval_anchor: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub last_accessed: Option<String>,
    pub deduplicated: bool,
    /// Short_id mentions found in note body, resolved to entities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved_mentions: Vec<ResolvedMention>,
    /// Tasks whose memory_refs contain this note's permalink.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<MemoryTaskRefItem>,
    /// Proposals reachable through this note's tasks/epics (via proposal_epics).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<MemoryProposalRefItem>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryConfirmResponse {
    pub note_id: Option<String>,
    pub permalink: Option<String>,
    pub previous_confidence: Option<f64>,
    pub new_confidence: Option<f64>,
    pub error: Option<String>,
}

impl MemoryConfirmResponse {
    pub fn error(error: String) -> Self {
        Self {
            note_id: None,
            permalink: None,
            previous_confidence: None,
            new_confidence: None,
            error: Some(error),
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemorySearchResultItem {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub folder: String,
    pub note_type: String,
    pub snippet: String,
    /// RRF fusion score (higher = more relevant). Defaults to 0.0 for backward compat.
    #[serde(default)]
    pub score: f64,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemorySearchResponse {
    pub results: Vec<MemorySearchResultItem>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryListResponse {
    pub notes: Vec<djinn_memory::NoteCompact>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryDeleteResponse {
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryHealthResponse {
    pub total_notes: Option<i64>,
    pub broken_link_count: Option<i64>,
    pub orphan_note_count: Option<i64>,
    pub low_confidence_note_count: Option<i64>,
    pub stale_note_count: Option<i64>,
    pub stale_notes_by_folder: Option<Vec<djinn_memory::StaleFolder>>,
    pub lifecycle: Option<djinn_memory::LifecycleHealth>,
    pub recent_sweep: Option<djinn_memory::RecentSweepMetrics>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryCatalogResponse {
    pub catalog: String,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryRecentResponse {
    pub notes: Vec<djinn_memory::NoteCompact>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryHistoryResponse {
    pub history: Vec<GitLogEntry>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryDiffResponse {
    pub diff: String,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryBuildContextResponse {
    pub primary: Vec<MemoryNoteView>,
    pub related_l1: Vec<djinn_memory::NoteOverview>,
    pub related_l0: Vec<djinn_memory::NoteAbstract>,
    /// Notes in the context set that are superseded by another note in the set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<djinn_memory::SupersedesAnnotation>,
    /// Notes in the context set that have a contradicting relationship.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contradicts: Vec<djinn_memory::ContradictsAnnotation>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryTaskRefsResponse {
    pub tasks: Vec<MemoryTaskRefItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<MemoryProposalRefItem>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct MemoryTaskRefItem {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct MemoryProposalRefItem {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryGraphResponse {
    pub nodes: Vec<djinn_memory::GraphNode>,
    pub edges: Vec<djinn_memory::GraphEdge>,
    /// Typed semantic edges (builds_on, contradicts, supersedes, exemplifies,
    /// derived_from) from `note_associations` where `kind <> 'co_access'`.
    #[serde(default)]
    pub typed_edges: Vec<djinn_memory::TypedEdge>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryBrokenLinksResponse {
    pub broken_links: Vec<djinn_memory::BrokenLink>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryOrphansResponse {
    pub orphans: Vec<djinn_memory::OrphanNote>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryNoteView {
    pub id: String,
    pub project_id: String,
    pub permalink: String,
    pub title: String,
    pub file_path: String,
    pub note_type: String,
    pub folder: String,
    pub status: String,
    pub tags: Vec<String>,
    pub content: String,
    pub retrieval_anchor: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed: String,
}

impl From<djinn_memory::Note> for MemoryNoteView {
    fn from(note: djinn_memory::Note) -> Self {
        Self {
            tags: note.parsed_tags(),
            id: note.id,
            project_id: note.project_id,
            permalink: note.permalink,
            title: note.title,
            file_path: note.file_path,
            note_type: note.note_type,
            folder: note.folder,
            status: note.status,
            content: note.content,
            retrieval_anchor: note.retrieval_anchor,
            created_at: note.created_at,
            updated_at: note.updated_at,
            last_accessed: note.last_accessed,
        }
    }
}

impl From<&djinn_memory::Note> for MemoryNoteView {
    fn from(note: &djinn_memory::Note) -> Self {
        Self {
            id: note.id.clone(),
            project_id: note.project_id.clone(),
            permalink: note.permalink.clone(),
            title: note.title.clone(),
            file_path: note.file_path.clone(),
            note_type: note.note_type.clone(),
            folder: note.folder.clone(),
            status: note.status.clone(),
            tags: note.parsed_tags(),
            content: note.content.clone(),
            retrieval_anchor: note.retrieval_anchor.clone(),
            created_at: note.created_at.clone(),
            updated_at: note.updated_at.clone(),
            last_accessed: note.last_accessed.clone(),
        }
    }
}

impl MemoryNoteResponse {
    pub fn from_note(note: &djinn_memory::Note) -> Self {
        Self::from_note_with_deduplicated(note, false)
    }

    pub fn deduplicated_from_note(note: &djinn_memory::Note) -> Self {
        Self::from_note_with_deduplicated(note, true)
    }

    fn from_note_with_deduplicated(note: &djinn_memory::Note, deduplicated: bool) -> Self {
        Self {
            id: Some(note.id.clone()),
            project_id: Some(note.project_id.clone()),
            permalink: Some(note.permalink.clone()),
            title: Some(note.title.clone()),
            file_path: Some(note.file_path.clone()),
            note_type: Some(note.note_type.clone()),
            folder: Some(note.folder.clone()),
            status: Some(note.status.clone()),
            tags: Some(note.parsed_tags()),
            content: Some(note.content.clone()),
            retrieval_anchor: note.retrieval_anchor.clone(),
            created_at: Some(note.created_at.clone()),
            updated_at: Some(note.updated_at.clone()),
            last_accessed: Some(note.last_accessed.clone()),
            deduplicated,
            resolved_mentions: vec![],
            tasks: vec![],
            proposals: vec![],
            error: None,
        }
    }

    pub(super) fn error(error: String) -> Self {
        Self {
            id: None,
            project_id: None,
            permalink: None,
            title: None,
            file_path: None,
            note_type: None,
            folder: None,
            status: None,
            tags: None,
            content: None,
            retrieval_anchor: None,
            created_at: None,
            updated_at: None,
            last_accessed: None,
            deduplicated: false,
            resolved_mentions: vec![],
            tasks: vec![],
            proposals: vec![],
            error: Some(error),
        }
    }
}
