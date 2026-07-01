//! Finalize tool payload types.
//!
//! Extracted from `djinn-agent::roles::finalize` so the slot crate can parse
//! finalize tool payloads without depending on djinn-agent.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AutoSubmitReviewMetadataPayload {
    pub task_run_id: String,
    pub trigger_reason: String,
    pub diff_fingerprint: String,
    pub verify_source: Option<String>,
    pub verify_run_id: Option<String>,
    pub verify_timestamp: Option<String>,
    pub session_id: Option<String>,
    pub model_id: Option<String>,
    #[serde(default)]
    pub no_progress_streak: i32,
}

/// Per-criterion verdict from a reviewer's `submit_review` call.
#[derive(Debug, Deserialize)]
pub struct AcVerdict {
    #[serde(default)]
    pub criterion: String,
    pub met: bool,
}

/// Entry from a planner's `submit_grooming` call.
#[derive(Debug, Deserialize)]
pub struct TaskGroomingEntry {
    pub task_id: String,
    pub action: String,
    pub changes: Option<String>,
}

/// Payload for a worker submitting completed work.
#[derive(Debug, Deserialize)]
pub struct SubmitWork {
    pub task_id: String,
    pub commit_title: String,
    pub summary: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub remaining_concerns: Vec<String>,
    #[serde(default)]
    pub auto_submit_review_metadata: Option<AutoSubmitReviewMetadataPayload>,
}

/// Payload for a reviewer submitting their review outcome.
#[derive(Debug, Deserialize)]
pub struct SubmitReview {
    pub task_id: String,
    pub verdict: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<AcVerdict>,
    pub feedback: Option<String>,
}

/// Payload for a Lead submitting an intervention decision.
#[derive(Debug, Deserialize)]
pub struct SubmitDecision {
    pub task_id: String,
    pub decision: String,
    pub rationale: Option<String>,
    #[serde(default)]
    pub created_tasks: Vec<String>,
}

/// Payload for a Planner submitting planning results.
#[derive(Debug, Deserialize)]
pub struct SubmitGrooming {
    #[serde(default)]
    pub tasks_reviewed: Vec<TaskGroomingEntry>,
    pub summary: Option<String>,
    pub decision: Option<String>,
}
