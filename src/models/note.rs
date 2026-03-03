use schemars::JsonSchema;
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

// ── Wikilink graph types ──────────────────────────────────────────────────────

/// A knowledge graph node (note with connection metadata).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GraphNode {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
    /// Total resolved edges incident to this node (inbound + outbound).
    pub connection_count: i64,
}

/// A resolved wikilink edge between two notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub raw_text: String,
}

/// Full knowledge graph: all nodes and all resolved edges.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GraphResponse {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// A wikilink pointing to a note that does not exist.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BrokenLink {
    pub source_id: String,
    pub source_permalink: String,
    pub source_title: String,
    pub raw_text: String,
}

/// A note with zero inbound wikilinks (potential dead-end).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct OrphanNote {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
}
