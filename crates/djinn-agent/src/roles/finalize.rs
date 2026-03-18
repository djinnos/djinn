use rmcp::model::Tool as RmcpTool;
use rmcp::object;
use serde::Deserialize;

/// Payload for a worker submitting completed work.
#[derive(Debug, Deserialize)]
pub struct SubmitWork {
    pub task_id: String,
    pub summary: String,
}

/// Payload for a reviewer submitting their review outcome.
#[derive(Debug, Deserialize)]
pub struct SubmitReview {
    pub task_id: String,
    pub approved: bool,
    pub comment: Option<String>,
}

/// Payload for a PM submitting an intervention decision.
#[derive(Debug, Deserialize)]
pub struct SubmitDecision {
    pub task_id: String,
    pub decision: String,
    pub rationale: Option<String>,
}

/// Payload for a groomer submitting grooming results.
#[derive(Debug, Deserialize)]
pub struct SubmitGrooming {
    pub summary: Option<String>,
}

/// MCP tool descriptor for the Worker finalize tool.
pub fn tool_submit_work() -> RmcpTool {
    RmcpTool::new(
        "submit_work".to_string(),
        "Signal that the worker has finished implementing the task. Provide a summary of changes made. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "summary"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "summary": {"type": "string", "description": "Brief summary of the work completed"}
            }
        }),
    )
}

/// MCP tool descriptor for the TaskReviewer finalize tool.
pub fn tool_submit_review() -> RmcpTool {
    RmcpTool::new(
        "submit_review".to_string(),
        "Submit the task review outcome. Approve or reject the task. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "approved"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "approved": {"type": "boolean", "description": "Whether the task passes review"},
                "comment": {"type": "string", "description": "Optional reviewer notes or rejection reason"}
            }
        }),
    )
}

/// MCP tool descriptor for the PM finalize tool.
pub fn tool_submit_decision() -> RmcpTool {
    RmcpTool::new(
        "submit_decision".to_string(),
        "Submit the PM intervention decision and release the task back to the worker queue. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "decision"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "decision": {"type": "string", "description": "The decision made (e.g. scope reduced, clarified, split into subtasks)"},
                "rationale": {"type": "string", "description": "Optional explanation for the decision"}
            }
        }),
    )
}

/// MCP tool descriptor for the Groomer finalize tool.
pub fn tool_submit_grooming() -> RmcpTool {
    RmcpTool::new(
        "submit_grooming".to_string(),
        "Signal that the grooming session is complete. Optionally summarise changes made to the backlog. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "summary": {"type": "string", "description": "Optional summary of grooming changes (tasks created, updated, or closed)"}
            }
        }),
    )
}
