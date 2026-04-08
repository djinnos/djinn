use serde::{Deserialize, Serialize};

/// Top-level grouping entity with a simplified open→closed lifecycle.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Epic {
    pub id: String,
    pub project_id: String,
    pub short_id: String,
    pub title: String,
    pub description: String,
    pub emoji: String,
    pub color: String,
    pub status: String,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub memory_refs: String,
    /// ADR-051 Epic C — when false, epic creation does not auto-dispatch
    /// a breakdown Planner.  Default true to preserve existing behaviour.
    pub auto_breakdown: bool,
    /// ADR-051 Epic C — slug of the accepted ADR that spawned this epic.
    /// Threaded into the breakdown Planner's session context.
    pub originating_adr_id: Option<String>,
}
