use serde::{Deserialize, Serialize};

/// Task board work item, always scoped under an epic.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub short_id: String,
    pub epic_id: String,
    pub title: String,
    pub description: String,
    pub design: String,
    pub issue_type: String,
    pub status: String,
    pub priority: i64,
    pub owner: String,
    /// JSON array of label strings.
    pub labels: String,
    /// JSON array of acceptance-criteria objects.
    pub acceptance_criteria: String,
    pub reopen_count: i64,
    pub continuation_count: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

/// A single entry in the task activity log (audit trail + comments).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: String,
    pub task_id: Option<String>,
    pub actor_id: String,
    pub actor_role: String,
    pub event_type: String,
    /// JSON payload — shape varies by event_type.
    pub payload: String,
    pub created_at: String,
}
