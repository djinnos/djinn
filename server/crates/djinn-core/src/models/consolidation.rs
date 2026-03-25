use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Lightweight DB-backed note identity used by consolidation queries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidationNoteRef {
    pub id: String,
    pub project_id: String,
    pub note_type: String,
    pub permalink: String,
    pub title: String,
    pub abstract_: Option<String>,
    pub overview: Option<String>,
}

/// One `(project_id, note_type)` bucket of DB-backed knowledge notes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidationNoteGroup {
    pub project_id: String,
    pub note_type: String,
    pub notes: Vec<ConsolidationNoteRef>,
}

/// A likely-duplicate edge between two notes inside a consolidation group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidationCandidateEdge {
    pub left_note_id: String,
    pub right_note_id: String,
    pub score: f64,
}

/// Deterministic connected component of likely-duplicate notes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidationCluster {
    pub project_id: String,
    pub note_type: String,
    pub notes: Vec<ConsolidationNoteRef>,
    pub edges: Vec<ConsolidationCandidateEdge>,
}
