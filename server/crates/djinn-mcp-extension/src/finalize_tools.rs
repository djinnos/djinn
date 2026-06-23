//! Finalize tool schema definitions (submit_work, submit_review, etc.).
//!
//! These are pure MCP tool descriptors — `RmcpTool` constructors with no
//! handler logic.  They live here so that [`tool_defs`] aggregation functions
//! can reference them without depending on `djinn-agent` internals.
//!
//! The corresponding payload struct types (`SubmitWork`, `SubmitReview`, …)
//! and handler dispatch remain in `djinn-agent::roles::finalize`.

use rmcp::model::Tool as RmcpTool;
use rmcp::object;

/// MCP tool descriptor for the Worker finalize tool.
pub fn tool_submit_work() -> RmcpTool {
    RmcpTool::new(
        "submit_work".to_string(),
        "Signal that the worker has finished implementing the task. Provide a summary of changes made and list of files modified. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "commit_title", "summary"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "commit_title": {"type": "string", "description": "Short imperative-mood git commit subject line, max 72 characters. Example: 'add rate limiting to auth middleware'", "maxLength": 72},
                "summary": {"type": "string", "description": "Longer description of the work completed, used as the commit body"},
                "files_changed": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "List of files modified during this session"
                },
                "remaining_concerns": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Any outstanding concerns or caveats for the reviewer"
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
        "Submit the Lead intervention decision. This is the ONLY way to end your session and is what applies the board transition — do not call task_transition yourself. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "decision"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "decision": {
                    "type": "string",
                    "enum": ["approve", "approve_conflict", "reopen", "decompose", "force_close", "escalate"],
                    "description": "The decision: approve (work is complete + correct — the worker just couldn't self-certify; this merges via the PR pipeline), approve_conflict (correct but a merge conflict exists — approve then send for conflict retry), reopen (send back to a fresh worker after you've rescoped/guided/added blockers), decompose (you created replacement subtasks — close the original), force_close (redundant or already-landed work), escalate (you cannot resolve it — return to the board for Planner/human review)"
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
                "summary": {"type": "string", "description": "Optional overall summary of the grooming session"},
                "decision": {
                    "type": "string",
                    "enum": ["execute", "close", "escalate"],
                    "description": "Outcome decision: 'execute' = wave was created or board work continues (coordinator dispatches the new tasks); 'close' = epic is complete and the planning task should close (set `reason` in summary); 'escalate' = board state needs human attention. Optional — defaults to 'execute' when omitted."
                }
            }
        }),
    )
}
