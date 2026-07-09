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

/// MCP tool descriptor for the Lead/arbiter finalize tool.
pub fn tool_submit_decision() -> RmcpTool {
    RmcpTool::new(
        "submit_decision".to_string(),
        "Submit the Lead/arbiter intervention decision. This is the ONLY way to end your session and is what applies the board transition — do not call task_transition yourself. Your session ends after this call.".to_string(),
        object!({
            "type": "object",
            "required": ["task_id", "decision"],
            "properties": {
                "task_id": {"type": "string", "description": "Task UUID or short_id"},
                "decision": {
                    "type": "string",
                    "enum": ["approve", "approve_conflict", "reopen", "park", "supersede"],
                    "description": "The decision: approve (work is complete + correct — the worker just couldn't self-certify; requires evidence citation), approve_conflict (correct but a merge conflict exists — approve then send for conflict retry; requires evidence citation), reopen (send back to a fresh worker with a directive and verification command), park (hold the task for human review with a structured dossier), supersede (you decomposed the work into replacement subtasks that carry it forward — the source task and its PR are force-closed as superseded automatically; requires a non-empty created_tasks array)"
                },
                "rationale": {"type": "string", "description": "Explanation for the decision"},
                "evidence": {
                    "type": "object",
                    "description": "Required for approve and approve_conflict. Evidence citation pointing at concrete verification/review/git evidence.",
                    "required": ["source", "summary"],
                    "properties": {
                        "source": {"type": "string", "description": "Evidence source type: 'ci_run', 'review_round', 'git_log', 'test_output', or 'manual_review'"},
                        "summary": {"type": "string", "description": "Human-readable summary of the evidence and why it supports the approval"},
                        "reference_id": {"type": "string", "description": "Optional reference ID (CI run ID, session ID, commit SHA, etc.)"}
                    }
                },
                "directive": {
                    "type": "string",
                    "description": "Required for reopen. Non-empty directive injected verbatim into the next worker prompt."
                },
                "verification_command": {
                    "type": "string",
                    "description": "Required for reopen. Executable command the next worker must run to verify the fix."
                },
                "exclude_models": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional for reopen. Model IDs excluded from the next dispatch."
                },
                "park_dossier": {
                    "type": "object",
                    "description": "Required for park. Structured dossier for the human-review hold.",
                    "required": ["hold_description", "failure_analysis"],
                    "properties": {
                        "hold_description": {"type": "string", "description": "Description of the hold for human reviewers"},
                        "failure_analysis": {"type": "string", "description": "Analysis of why the task failed and needs human review"},
                        "attempted_decisions": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional list of decisions attempted before parking"
                        },
                        "recommended_action": {"type": "string", "description": "Optional recommended action for the human reviewer"}
                    }
                },
                "created_tasks": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Required for supersede: IDs (short_id or UUID) of the replacement subtasks created during this intervention that carry the work forward. Must be non-empty for a supersede decision."
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
                },
                "blocked_on": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Epics (UUID or short_id) that block this epic. If you conclude the epic cannot proceed until another epic closes and you therefore created NO worker tasks, list the blocking epic(s) here (use decision='escalate'). This durably parks this epic's planning until every listed epic closes — the coordinator wakes it automatically then. Reference the blocking EPIC, not a task (if a task blocks you, reference the epic that owns it). Do NOT re-run planning to restate a known block: declare it here once."
                }
            }
        }),
    )
}
