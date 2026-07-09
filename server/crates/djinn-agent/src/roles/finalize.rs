use serde::Deserialize;

// Re-export tool schema definitions from djinn-mcp-extension so that
// existing `crate::roles::finalize::tool_submit_*()` call sites compile
// unchanged.  The struct types (AcVerdict, SubmitWork, …) and handler
// dispatch remain in this crate.
pub use djinn_mcp_extension::finalize_tools::{
    tool_submit_decision, tool_submit_grooming, tool_submit_review, tool_submit_work,
};

/// Per-criterion verdict from a reviewer's `submit_review` call.
#[derive(Debug, Deserialize)]
pub struct AcVerdict {
    /// Text of the criterion being judged. May be empty if the agent omits it;
    /// the handler falls back to the existing criterion text from the task.
    #[serde(default)]
    pub criterion: String,
    pub met: bool,
}

/// Entry from a planner's `submit_grooming` call.
#[derive(Debug, Deserialize)]
pub struct TaskGroomingEntry {
    pub task_id: String,
    /// Action taken: "promoted", "improved", or "skipped".
    pub action: String,
    /// Human-readable description of changes made to this task.
    pub changes: Option<String>,
}

/// Payload for a worker submitting completed work.
#[derive(Debug, Deserialize)]
pub struct SubmitWork {
    pub task_id: String,
    /// Short imperative-mood commit subject line (max 72 chars).
    pub commit_title: String,
    pub summary: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub remaining_concerns: Vec<String>,
}

/// Payload for a reviewer submitting their review outcome.
#[derive(Debug, Deserialize)]
pub struct SubmitReview {
    pub task_id: String,
    /// Explicit verdict: "approved" or "rejected".
    pub verdict: String,
    /// Per-criterion verdicts used to atomically set AC met/unmet state on the task.
    #[serde(default)]
    pub acceptance_criteria: Vec<AcVerdict>,
    /// Feedback or rejection reason logged as structured activity.
    pub feedback: Option<String>,
}

/// Payload for a Lead/arbiter submitting an intervention decision.
#[derive(Debug, Deserialize)]
pub struct SubmitDecision {
    pub task_id: String,
    /// Decision taken: "approve", "approve_conflict", "reopen", "park", or
    /// "supersede". The supervisor maps this to the terminal board transition
    /// (see `StageOutcome` Lead variants); the Lead does NOT
    /// call `task_transition` for the terminal move itself.
    pub decision: String,
    pub rationale: Option<String>,
    /// Evidence citation — required for `approve` and `approve_conflict`.
    pub evidence: Option<serde_json::Value>,
    /// Park dossier — required for `park`.
    pub park_dossier: Option<serde_json::Value>,
    /// Directive — required for `reopen`.
    pub directive: Option<String>,
    /// Verification command — required for `reopen`.
    pub verification_command: Option<String>,
    /// Models excluded from next dispatch — optional for `reopen`.
    #[serde(default)]
    pub exclude_models: Vec<String>,
    /// IDs of replacement subtasks created during this Lead intervention.
    /// Required (non-empty) for the `supersede` decision — they are the tasks
    /// that carry the work forward when the source task is force-closed.
    #[serde(default)]
    pub created_tasks: Vec<String>,
}

/// Payload for a Planner submitting planning results.
#[derive(Debug, Deserialize)]
pub struct SubmitGrooming {
    /// Per-task planning entries.
    #[serde(default)]
    pub tasks_reviewed: Vec<TaskGroomingEntry>,
    /// Optional overall summary of the grooming session.
    pub summary: Option<String>,
    /// Outcome decision read by the supervisor: "execute" (dispatch the wave),
    /// "close" (epic complete, close the planning task), or "escalate" (board
    /// state needs human attention). Optional — supervisor defaults to
    /// "execute" when omitted so a missing field doesn't loop the planner.
    pub decision: Option<String>,
}

// Tool schema definitions are now re-exported from djinn-mcp-extension above.
