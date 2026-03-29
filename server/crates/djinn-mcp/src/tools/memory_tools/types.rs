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
    pub tags: Option<Vec<String>>,
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
    /// New note type to move the note to.
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
pub struct ReindexParams {
    pub project: String,
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
    pub co_access_count: i64,
    pub last_co_access: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryAssociationsResponse {
    pub associations: Vec<MemoryAssociationEntry>,
    pub error: Option<String>,
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
    pub tags: Option<Vec<String>>,
    pub content: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub last_accessed: Option<String>,
    pub deduplicated: bool,
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
    pub notes: Vec<djinn_core::models::NoteCompact>,
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
    pub stale_notes_by_folder: Option<Vec<djinn_core::models::StaleFolder>>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryCatalogResponse {
    pub catalog: String,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryRecentResponse {
    pub notes: Vec<djinn_core::models::NoteCompact>,
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
    pub related_l1: Vec<djinn_core::models::NoteOverview>,
    pub related_l0: Vec<djinn_core::models::NoteAbstract>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryTaskRefsResponse {
    pub tasks: Vec<MemoryTaskRefItem>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct MemoryTaskRefItem {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryGraphResponse {
    pub nodes: Vec<djinn_core::models::GraphNode>,
    pub edges: Vec<djinn_core::models::GraphEdge>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryBrokenLinksResponse {
    pub broken_links: Vec<djinn_core::models::BrokenLink>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryOrphansResponse {
    pub orphans: Vec<djinn_core::models::OrphanNote>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryReindexResponse {
    pub updated: i64,
    pub created: i64,
    pub deleted: i64,
    pub unchanged: i64,
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
    pub tags: Vec<String>,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed: String,
}

impl From<djinn_core::models::Note> for MemoryNoteView {
    fn from(note: djinn_core::models::Note) -> Self {
        Self {
            tags: note.parsed_tags(),
            id: note.id,
            project_id: note.project_id,
            permalink: note.permalink,
            title: note.title,
            file_path: note.file_path,
            note_type: note.note_type,
            folder: note.folder,
            content: note.content,
            created_at: note.created_at,
            updated_at: note.updated_at,
            last_accessed: note.last_accessed,
        }
    }
}

impl From<&djinn_core::models::Note> for MemoryNoteView {
    fn from(note: &djinn_core::models::Note) -> Self {
        Self {
            id: note.id.clone(),
            project_id: note.project_id.clone(),
            permalink: note.permalink.clone(),
            title: note.title.clone(),
            file_path: note.file_path.clone(),
            note_type: note.note_type.clone(),
            folder: note.folder.clone(),
            tags: note.parsed_tags(),
            content: note.content.clone(),
            created_at: note.created_at.clone(),
            updated_at: note.updated_at.clone(),
            last_accessed: note.last_accessed.clone(),
        }
    }
}

impl MemoryNoteResponse {
    pub fn from_note(note: &djinn_core::models::Note) -> Self {
        Self::from_note_with_deduplicated(note, false)
    }

    pub fn deduplicated_from_note(note: &djinn_core::models::Note) -> Self {
        Self::from_note_with_deduplicated(note, true)
    }

    fn from_note_with_deduplicated(note: &djinn_core::models::Note, deduplicated: bool) -> Self {
        Self {
            id: Some(note.id.clone()),
            project_id: Some(note.project_id.clone()),
            permalink: Some(note.permalink.clone()),
            title: Some(note.title.clone()),
            file_path: Some(note.file_path.clone()),
            note_type: Some(note.note_type.clone()),
            folder: Some(note.folder.clone()),
            tags: Some(note.parsed_tags()),
            content: Some(note.content.clone()),
            created_at: Some(note.created_at.clone()),
            updated_at: Some(note.updated_at.clone()),
            last_accessed: Some(note.last_accessed.clone()),
            deduplicated,
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
            tags: None,
            content: None,
            created_at: None,
            updated_at: None,
            last_accessed: None,
            deduplicated: false,
            error: Some(error),
        }
    }
}
