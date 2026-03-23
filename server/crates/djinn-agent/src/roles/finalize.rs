use rmcp::model::Tool as RmcpTool;
use rmcp::object;
use serde::Deserialize;

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
    pub summary: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub remaining_concerns: Vec<String>,
    /// Optional: minutes until the next patrol should run (architect self-scheduling).
    pub next_patrol_minutes: Option<u32>,
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

/// Payload for a Lead submitting an intervention decision.
#[derive(Debug, Deserialize)]
pub struct SubmitDecision {
    pub task_id: String,
    /// Decision taken: "reopen", "decompose", "force_close", or "escalate".
    pub decision: String,
    pub rationale: Option<String>,
    /// IDs of tasks created during this Lead intervention (for decompose decisions).
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
}

/// MCP tool descriptor for the Worker finalize tool.
pub fn tool_submit_work() -> RmcpTool {
    RmcpTool::new(
        "submit_work".to_string(),
        "Signal that the worker has finished implementing the task. Provide a summary of changes made and list of files modified. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "summary"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "summary": {"type": "string", "description": "Brief summary of the work completed"},
                "files_changed": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of files modified during this session"
                },
                "remaining_concerns": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Any outstanding concerns or caveats for the reviewer"
                },
                "next_patrol_minutes": {
                    "type": "integer",
                    "description": "Architect only: minutes until the next patrol should run (5-60). Omit for non-architect roles."
                }
            }
        }),
    )
}

/// MCP tool descriptor for the Reviewer finalize tool.
pub fn tool_submit_review() -> RmcpTool {
    RmcpTool::new(
        "submit_review".to_string(),
        "Submit the task review outcome with per-criterion AC verdicts. This atomically sets acceptance criteria met/unmet state on the task. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "verdict"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "verdict": {
                    "type": "string",
                    "enum": ["approved", "rejected"],
                    "description": "Overall review verdict"
                },
                "acceptance_criteria": {
                    "type": "array",
                    "description": "Per-criterion verdicts that atomically set AC met/unmet state on the task",
                    "items": {
                        "type": "object",
                        "required": ["criterion", "met"],
                        "properties": {
                            "criterion": {"type": "string", "description": "Text of the criterion"},
                            "met": {"type": "boolean", "description": "Whether this criterion is met"}
                        }
                    }
                },
                "feedback": {"type": "string", "description": "Feedback or rejection reason for the worker"}
            }
        }),
    )
}

/// MCP tool descriptor for the Lead finalize tool.
pub fn tool_submit_decision() -> RmcpTool {
    RmcpTool::new(
        "submit_decision".to_string(),
        "Submit the Lead intervention decision and release the task back to the worker queue. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "decision"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "decision": {
                    "type": "string",
                    "enum": ["reopen", "decompose", "force_close", "escalate"],
                    "description": "The decision taken: reopen (send back to worker), decompose (split into subtasks), force_close, or escalate (release back to Lead queue)"
                },
                "rationale": {"type": "string", "description": "Explanation for the decision"},
                "created_tasks": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "IDs of tasks created during this intervention (for decompose decisions)"
                }
            }
        }),
    )
}

/// MCP tool descriptor for the Planner finalize tool.
pub fn tool_submit_grooming() -> RmcpTool {
    RmcpTool::new(
        "submit_grooming".to_string(),
        "Signal that the grooming session is complete. Report per-task actions taken. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "tasks_reviewed": {
                    "type": "array",
                    "description": "Per-task grooming entries",
                    "items": {
                        "type": "object",
                        "required": ["task_id", "action"],
                        "properties": {
                            "task_id": {"type": "string", "description": "Task UUID or short_id"},
                            "action": {
                                "type": "string",
                                "enum": ["promoted", "improved", "skipped"],
                                "description": "Action taken on this task"
                            },
                            "changes": {"type": "string", "description": "Description of changes made to this task"}
                        }
                    }
                },
                "summary": {"type": "string", "description": "Optional overall summary of the grooming session"}
            }
        }),
    )
}
