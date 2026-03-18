use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A knowledge base note. Source of truth is the markdown file on disk;
/// this struct represents the SQLite index row.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
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
    pub tags: String,    // JSON array string, e.g. '["rust","db"]'
    pub content: String, // Markdown body without frontmatter
    pub created_at: String,
    pub updated_at: String,
    pub last_accessed: String,
    pub access_count: i64,
    pub confidence: f64,
    pub abstract_: Option<String>,
    pub overview: Option<String>,
}

impl Note {
    /// Parse the JSON tags string into a `Vec<String>`.
    pub fn parsed_tags(&self) -> Vec<String> {
        serde_json::from_str(&self.tags).unwrap_or_default()
    }

    /// Convert to a `serde_json::Value` with parsed tags.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "project_id": self.project_id,
            "permalink": self.permalink,
            "title": self.title,
            "file_path": self.file_path,
            "note_type": self.note_type,
            "folder": self.folder,
            "tags": self.parsed_tags(),
            "content": self.content,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "last_accessed": self.last_accessed,
            "access_count": self.access_count,
            "confidence": self.confidence,
            "abstract": self.abstract_,
            "overview": self.overview,
        })
    }
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

/// Compact note summary (no full content) for list and recent queries.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NoteCompact {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
    pub updated_at: String,
}

/// L1 note overview payload used in tiered context responses.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NoteOverview {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub overview_text: String,
    pub score: Option<f32>,
}

/// L0 note abstract payload used in tiered context responses.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NoteAbstract {
    pub id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub abstract_text: String,
    pub score: Option<f32>,
}

/// A single git commit entry for note history.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GitLogEntry {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub date: String,
}

/// Health report for a project's knowledge base.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct HealthReport {
    pub total_notes: i64,
    pub broken_link_count: i64,
    pub orphan_note_count: i64,
    pub stale_notes_by_folder: Vec<StaleFolder>,
}

/// Stale-note count for one folder.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct StaleFolder {
    pub folder: String,
    pub count: i64,
}

/// Context built from a seed note + linked related notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BuildContextResponse {
    /// Full-content notes at the seed.
    pub primary: Vec<Note>,
    /// L1 overview notes reached by first-hop link traversal.
    pub related_l1: Vec<NoteOverview>,
    /// L0 abstract notes reached by deeper link traversal.
    pub related_l0: Vec<NoteAbstract>,
}

/// Result of a filesystem-to-index reconciliation pass.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReindexSummary {
    pub updated: i64,
    pub created: i64,
    pub deleted: i64,
    pub unchanged: i64,
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
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
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

#[cfg(test)]
mod tests {
    use super::{NoteAbstract, NoteOverview};

    #[test]
    fn note_overview_serializes_with_stable_field_names() {
        let payload = NoteOverview {
            id: "note_123".to_string(),
            permalink: "reference/example".to_string(),
            title: "Example Note".to_string(),
            note_type: "reference".to_string(),
            overview_text: "Short summary".to_string(),
            score: Some(0.87),
        };

        let value = serde_json::to_value(payload).expect("serializes to JSON value");
        assert_eq!(value["id"], "note_123");
        assert_eq!(value["permalink"], "reference/example");
        assert_eq!(value["title"], "Example Note");
        assert_eq!(value["note_type"], "reference");
        assert_eq!(value["overview_text"], "Short summary");
        assert_eq!(value["score"].as_f64(), Some(0.8700000047683716));
    }

    #[test]
    fn note_abstract_serializes_with_stable_field_names() {
        let payload = NoteAbstract {
            id: "note_456".to_string(),
            permalink: "reference/another".to_string(),
            title: "Another Note".to_string(),
            note_type: "reference".to_string(),
            abstract_text: "L0 abstract".to_string(),
            score: None,
        };

        let value = serde_json::to_value(payload).expect("serializes to JSON value");
        assert_eq!(value["id"], "note_456");
        assert_eq!(value["permalink"], "reference/another");
        assert_eq!(value["title"], "Another Note");
        assert_eq!(value["note_type"], "reference");
        assert_eq!(value["abstract_text"], "L0 abstract");
        assert!(value.get("score").is_some());
        assert!(value["score"].is_null());
    }
}
