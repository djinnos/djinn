use serde::{Deserialize, Serialize};

/// Immutable observations for one exact task run. Missing historical rows are
/// conservatively `legacy_unknown`; these fields are never inferred later.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct TaskRunOutcomeFact {
    pub task_run_id: String,
    pub attempt_seq: Option<i32>,
    pub outcome: String,
    pub parked_reason: Option<String>,
    pub review_verdict: Option<String>,
    pub merge_queue_result: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
