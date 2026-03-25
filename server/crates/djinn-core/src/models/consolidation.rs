use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Source-session provenance entry for a consolidated DB-backed knowledge note.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidatedNoteProvenance {
    pub note_id: String,
    pub session_id: String,
    pub created_at: String,
}

/// Persisted metrics for a single consolidation worker run.
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ConsolidationRunMetric {
    pub id: String,
    pub project_id: String,
    pub note_type: String,
    pub status: String,
    pub scanned_note_count: i64,
    pub candidate_cluster_count: i64,
    pub consolidated_cluster_count: i64,
    pub consolidated_note_count: i64,
    pub source_note_count: i64,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub error_message: Option<String>,
}
