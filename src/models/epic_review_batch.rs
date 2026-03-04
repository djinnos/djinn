use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct EpicReviewBatch {
    pub id: String,
    pub project_id: String,
    pub epic_id: String,
    pub status: String,
    pub verdict_reason: Option<String>,
    pub session_id: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct EpicReviewBatchTask {
    pub batch_id: String,
    pub task_id: String,
    pub created_at: String,
}
