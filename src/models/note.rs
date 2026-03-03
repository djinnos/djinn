use serde::{Deserialize, Serialize};

/// A knowledge base note. Source of truth is the markdown file on disk;
/// this struct represents the SQLite index row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub project_id: String,
    /// Slug path unique within a project, e.g. "decisions/my-adr".
    pub permalink: String,
    pub title: String,
    /// Absolute path to the markdown file on disk.
    pub file_path: String,
    pub note_type: String,
    pub folder: String,
    pub tags: String,      // JSON array string, e.g. '["rust","db"]'
    pub content: String,   // Markdown body without frontmatter
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed: String,
}

/// A compact search result from FTS5 with BM25 score and content snippet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoteSearchResult {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub folder: String,
    pub note_type: String,
    /// HTML snippet with `<b>…</b>` highlights around matched terms.
    pub snippet: String,
}
