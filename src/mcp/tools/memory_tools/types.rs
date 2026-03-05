use super::*;

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    /// Absolute path to the project directory.
    pub project: String,
    pub title: String,
    pub content: String,
    /// Note type: adr, pattern, research, requirement, reference, design,
    /// session, persona, journey, design_spec, competitive, tech_spike,
    /// brief (singleton), roadmap (singleton).
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
    pub folder: String,
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
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemorySearchResultItem {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub folder: String,
    pub note_type: String,
    pub snippet: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemorySearchResponse {
    pub results: Vec<MemorySearchResultItem>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryListResponse {
    pub notes: Vec<crate::models::note::NoteCompact>,
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
    pub stale_notes_by_folder: Option<Vec<crate::models::note::StaleFolder>>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryCatalogResponse {
    pub catalog: String,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryRecentResponse {
    pub notes: Vec<crate::models::note::NoteCompact>,
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
    pub related: Vec<crate::models::note::NoteCompact>,
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
    pub nodes: Vec<crate::models::note::GraphNode>,
    pub edges: Vec<crate::models::note::GraphEdge>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryBrokenLinksResponse {
    pub broken_links: Vec<crate::models::note::BrokenLink>,
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct MemoryOrphansResponse {
    pub orphans: Vec<crate::models::note::OrphanNote>,
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

impl MemoryNoteResponse {
    pub(super) fn from_note(note: &crate::models::note::Note) -> Self {
        Self {
            id: Some(note.id.clone()),
            project_id: Some(note.project_id.clone()),
            permalink: Some(note.permalink.clone()),
            title: Some(note.title.clone()),
            file_path: Some(note.file_path.clone()),
            note_type: Some(note.note_type.clone()),
            folder: Some(note.folder.clone()),
            tags: Some(parse_tags_json(&note.tags)),
            content: Some(note.content.clone()),
            created_at: Some(note.created_at.clone()),
            updated_at: Some(note.updated_at.clone()),
            last_accessed: Some(note.last_accessed.clone()),
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
            error: Some(error),
        }
    }
}
