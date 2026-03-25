use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Minimal group descriptor for DB-backed knowledge-note consolidation scans.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DbNoteGroup {
    pub project_id: String,
    pub note_type: String,
    pub note_count: i64,
}

/// Compact DB-backed note payload used by consolidation queries.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ConsolidationNote {
    pub id: String,
    pub project_id: String,
    pub permalink: String,
    pub title: String,
    pub note_type: String,
    pub folder: String,
    pub content: String,
    pub abstract_: Option<String>,
    pub overview: Option<String>,
    pub confidence: f64,
}

/// Weighted likely-duplicate edge between two DB-backed notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ConsolidationCandidateEdge {
    pub left_note_id: String,
    pub right_note_id: String,
    pub score: f64,
}

/// Deterministic connected component of likely-duplicate DB-backed notes.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ConsolidationCluster {
    pub note_ids: Vec<String>,
    pub notes: Vec<ConsolidationNote>,
    pub edges: Vec<ConsolidationCandidateEdge>,
}
