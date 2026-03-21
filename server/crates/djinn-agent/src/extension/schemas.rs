use rmcp::model::Tool as RmcpTool;
use rmcp::object;

use super::*;

fn tool_task_show() -> RmcpTool {
    RmcpTool::new(
        "task_show".to_string(),
        "Show details of a work item including recent activity and blockers.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_request_lead() -> RmcpTool {
    RmcpTool::new(
        "request_lead".to_string(),
        "Request Lead intervention for the current task. Use when the task is too large to complete reliably, the design is ambiguous, or you are stuck. Adds a comment with your reason and suggested breakdown, then escalates to the Lead queue. Your session will effectively end after this call."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short_id"},
                "reason": {"type": "string", "description": "Why Lead intervention is needed (e.g. task too large, design ambiguous, blocked on decision)"},
                "suggested_breakdown": {"type": "string", "description": "Optional suggested split: list of smaller tasks the Lead should create"}
            }
        }),
    )
}

fn tool_request_architect() -> RmcpTool {
    RmcpTool::new(
        "request_architect".to_string(),
        "Escalate to the Architect when the task requires strategic technical review that is beyond Lead intervention scope. Use when the problem is architectural, requires codebase-wide analysis, or has failed multiple Lead interventions. Adds a comment and dispatches the Architect. Your session should end after this call."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short_id"},
                "reason": {"type": "string", "description": "Why Architect escalation is needed (e.g. architectural ambiguity, repeated Lead failures, codebase-wide impact)"}
            }
        }),
    )
}

fn tool_memory_read() -> RmcpTool {
    RmcpTool::new(
        "memory_read".to_string(),
        "Read a note by permalink or title.".to_string(),
        object!({
            "type": "object",
            "required": ["identifier"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "identifier": {"type": "string"}
            }
        }),
    )
}

fn tool_memory_search() -> RmcpTool {
    RmcpTool::new(
        "memory_search".to_string(),
        "Search notes in project memory.".to_string(),
        object!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "query": {"type": "string"},
                "folder": {"type": "string"},
                "type": {"type": "string"},
                "limit": {"type": "integer"},
                "task_id": {"type": "string", "description": "Task ID for affinity scoring; defaults to the current session task"}
            }
        }),
    )
}

fn tool_memory_list() -> RmcpTool {
    RmcpTool::new(
        "memory_list".to_string(),
        "List notes in project memory. Returns compact summaries without full content.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "folder": {"type": "string", "description": "Filter by folder (e.g. \"decisions\")"},
                "type": {"type": "string", "description": "Filter by note type (e.g. \"adr\", \"reference\", \"research\")"},
                "depth": {"type": "integer", "description": "Depth control: 0 = unlimited, 1 = exact folder (default), N = N levels"}
            }
        }),
    )
}

fn tool_memory_build_context() -> RmcpTool {
    RmcpTool::new(
        "memory_build_context".to_string(),
        "Build context from a memory note with progressive disclosure. Returns full content for primary notes, overview for direct linked notes, abstract for discovered related notes. Use url='folder/*' to return all notes in a folder. Seed notes are never dropped by budget constraints.".to_string(),
        object!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "url": {"type": "string", "description": "Memory URI: 'memory://folder/note', 'folder/note', or 'folder/*' for all notes in a folder"},
                "depth": {"type": "integer", "description": "Link traversal depth (default 1)"},
                "max_related": {"type": "integer", "description": "Maximum related notes to return (default 10)"},
                "budget": {"type": "integer", "description": "Token budget for context (default 4096)"},
                "task_id": {"type": "string", "description": "Task ID for affinity scoring"}
            }
        }),
    )
}

fn tool_role_metrics() -> RmcpTool {
    RmcpTool::new(
        "role_metrics".to_string(),
        "Return aggregated effectiveness metrics per agent role: success_rate, avg_tokens, avg_time_seconds, verification_pass_rate, avg_reopens, completed_task_count. Omit role_id to return metrics for all roles in the project.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "role_id": {"type": "string", "description": "Role UUID or name; omit for all roles"},
                "window_days": {"type": "integer", "description": "Days back for session data (default 30)"}
            }
        }),
    )
}

fn tool_role_amend_prompt() -> RmcpTool {
    RmcpTool::new(
        "role_amend_prompt".to_string(),
        "Append a prompt amendment to a specialist agent role's learned_prompt. The amendment is appended after existing content (never replacing it) and logged to learned_prompt_history. Only applicable to specialist roles (worker, reviewer, resolver base_role). Do NOT use on architect, lead, or planner roles.".to_string(),
        object!({
            "type": "object",
            "required": ["role_id", "amendment"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "role_id": {"type": "string", "description": "Role UUID or name to amend"},
                "amendment": {"type": "string", "description": "Amendment text to append to learned_prompt"},
                "metrics_snapshot": {"type": "string", "description": "JSON string of current metrics for the history record"}
            }
        }),
    )
}

fn tool_shell() -> RmcpTool {
    RmcpTool::new(
        "shell".to_string(),
        "Execute shell commands in the task worktree. Commands always run from the worktree root."
            .to_string(),
        object!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 120000, max 600000)"}
            }
        }),
    )
}

fn tool_read() -> RmcpTool {
    RmcpTool::new(
        "read".to_string(),
        "Read a file with line numbers and pagination. Rejects binary files.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["file_path"]
        }),
    )
}

fn tool_write() -> RmcpTool {
    RmcpTool::new(
        "write".to_string(),
        "Write content to a file, creating it or overwriting if it exists. Path must be within the task worktree.".to_string(),
        object!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string", "description": "Absolute or worktree-relative file path"},
                "content": {"type": "string", "description": "File content to write"}
            }
        }),
    )
}

fn tool_edit() -> RmcpTool {
    RmcpTool::new(
        "edit".to_string(),
        "Edit a file by replacing exact text. Finds old_text and replaces with new_text. Fails if old_text is not found or is ambiguous (appears multiple times).".to_string(),
        object!({
            "type": "object",
            "required": ["path", "old_text", "new_text"],
            "properties": {
                "path": {"type": "string", "description": "Absolute or worktree-relative file path"},
                "old_text": {"type": "string", "description": "Exact text to find and replace"},
                "new_text": {"type": "string", "description": "Replacement text"}
            }
        }),
    )
}

fn tool_task_create() -> RmcpTool {
    RmcpTool::new(
        "task_create".to_string(),
        "Create a new task under an epic. Use blocked_by to set dependencies and acceptance_criteria to define success criteria at creation.".to_string(),
        object!({
            "type": "object",
            "required": ["epic_id", "title"],
            "properties": {
                "epic_id": {"type": "string"},
                "title": {"type": "string"},
                "issue_type": {"type": "string"},
                "description": {"type": "string"},
                "design": {"type": "string"},
                "priority": {"type": "integer"},
                "owner": {"type": "string"},
                "status": {"type": "string", "description": "Optional initial status. Allowed: open (default)."},
                "acceptance_criteria": {"type": "array", "items": {"type": "string"}, "description": "List of acceptance criteria strings."},
                "blocked_by": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) that block this task."},
                "memory_refs": {"type": "array", "items": {"type": "string"}, "description": "Memory note permalinks to attach."},
                "agent_type": {"type": "string", "description": "Specialist role name to route this task (e.g. 'rust-expert'). Must match a configured AgentRole name for this project."}
            }
        }),
    )
}

fn tool_task_update() -> RmcpTool {
    RmcpTool::new(
        "task_update".to_string(),
        "Update task fields: title, description, design, priority, owner, labels, acceptance_criteria, memory_refs, blocked_by.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "design": {"type": "string"},
                "priority": {"type": "integer"},
                "owner": {"type": "string"},
                "labels_add": {"type": "array", "items": {"type": "string"}},
                "labels_remove": {"type": "array", "items": {"type": "string"}},
                "acceptance_criteria": {"type": "array", "items": {"type": "object"}},
                "memory_refs_add": {"type": "array", "items": {"type": "string"}},
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}},
                "blocked_by_add": {"type": "array", "items": {"type": "string"}, "description": "Task IDs to add as blockers"},
                "blocked_by_remove": {"type": "array", "items": {"type": "string"}, "description": "Task IDs to remove as blockers"}
            }
        }),
    )
}

fn tool_task_transition() -> RmcpTool {
    RmcpTool::new(
        "task_transition".to_string(),
        "Execute a state machine transition on a task. Lead can use: lead_intervention_complete (rescope and reopen for worker), lead_approve (approve implementation and merge), force_close (requires replacement_task_ids for decomposition OR reason for redundant/already-landed tasks), escalate.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "action"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "action": {"type": "string", "description": "Transition action"},
                "reason": {"type": "string"},
                "target_status": {"type": "string"},
                "replacement_task_ids": {"type": "array", "items": {"type": "string"}, "description": "For force_close with decomposition: IDs of replacement subtasks. Not required if closing a redundant task with a reason."}
            }
        }),
    )
}

fn tool_task_delete_branch() -> RmcpTool {
    RmcpTool::new(
        "task_delete_branch".to_string(),
        "Delete the task's git branch, worktree, and paused session so the next worker starts with a clean slate.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_task_archive_activity() -> RmcpTool {
    RmcpTool::new(
        "task_archive_activity".to_string(),
        "Soft-delete all activity entries (comments, session errors, rejections) for a task. The worker on the next attempt will only see post-intervention activity.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_task_reset_counters() -> RmcpTool {
    RmcpTool::new(
        "task_reset_counters".to_string(),
        "Reset reopen_count and continuation_count to zero. Use when the task has been meaningfully rescoped and old retry history is no longer relevant.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_task_kill_session() -> RmcpTool {
    RmcpTool::new(
        "task_kill_session".to_string(),
        "Kill the paused worker session and delete its saved conversation. The next dispatch will start a fresh session. Unlike task_delete_branch, this preserves the branch and any committed code.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_task_comment_add() -> RmcpTool {
    RmcpTool::new(
        "task_comment_add".to_string(),
        "Add a comment or strategic observation to a task's activity log.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "body"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "body": {"type": "string", "description": "Comment body to add to the activity log"}
            }
        }),
    )
}

fn tool_output_view() -> RmcpTool {
    RmcpTool::new(
        "output_view".to_string(),
        "Paginated view of a truncated tool output. When a tool result was truncated, \
         the full output is stashed and can be browsed here by tool_use_id."
            .to_string(),
        object!({
            "type": "object",
            "required": ["tool_use_id"],
            "properties": {
                "tool_use_id": {"type": "string", "description": "The tool_use_id from the truncated result"},
                "offset": {"type": "integer", "minimum": 0, "description": "Line offset (0-based, default 0)"},
                "limit": {"type": "integer", "minimum": 1, "description": "Number of lines to return (default 200)"}
            }
        }),
    )
}

fn tool_output_grep() -> RmcpTool {
    RmcpTool::new(
        "output_grep".to_string(),
        "Regex search within a truncated tool output. Returns matching lines with \
         context from the full stashed output."
            .to_string(),
        object!({
            "type": "object",
            "required": ["tool_use_id", "pattern"],
            "properties": {
                "tool_use_id": {"type": "string", "description": "The tool_use_id from the truncated result"},
                "pattern": {"type": "string", "description": "Regex pattern to search for"},
                "context_lines": {"type": "integer", "minimum": 0, "description": "Lines of context around each match (default 3)"}
            }
        }),
    )
}

fn base_tool_schemas() -> Vec<serde_json::Value> {
    let mut tool_values = vec![
        serde_json::to_value(tool_task_show()).expect("serialize tool_task_show"),
        serde_json::to_value(tool_task_list()).expect("serialize tool_task_list"),
        serde_json::to_value(tool_task_activity_list()).expect("serialize tool_task_activity_list"),
        serde_json::to_value(tool_memory_read()).expect("serialize tool_memory_read"),
        serde_json::to_value(tool_memory_search()).expect("serialize tool_memory_search"),
        serde_json::to_value(tool_memory_list()).expect("serialize tool_memory_list"),
    ];
    tool_values.push(serde_json::to_value(tool_shell()).expect("serialize tool_shell"));
    tool_values.push(serde_json::to_value(tool_read()).expect("serialize tool_read"));
    tool_values.push(serde_json::to_value(tool_lsp()).expect("serialize tool_lsp"));
    tool_values.push(serde_json::to_value(tool_output_view()).expect("serialize tool_output_view"));
    tool_values.push(serde_json::to_value(tool_output_grep()).expect("serialize tool_output_grep"));
    tool_values
}

/// Tool schemas for Worker and Resolver: base + file-editing tools.
pub(crate) fn tool_schemas_worker() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serde_json::to_value(tool_write()).expect("serialize tool_write"));
    tool_values.push(serde_json::to_value(tool_edit()).expect("serialize tool_edit"));
    tool_values.push(serde_json::to_value(tool_apply_patch()).expect("serialize tool_apply_patch"));
    tool_values
        .push(serde_json::to_value(tool_request_lead()).expect("serialize tool_request_lead"));
    tool_values.push(
        serde_json::to_value(crate::roles::finalize::tool_submit_work())
            .expect("serialize tool_submit_work"),
    );
    tool_values
}

/// Tool schemas for Reviewer: base + submit_review finalize tool.
/// task_update_ac is excluded — submit_review sets AC atomically.
pub(crate) fn tool_schemas_reviewer() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(
        serde_json::to_value(crate::roles::finalize::tool_submit_review())
            .expect("serialize tool_submit_review"),
    );
    tool_values
}

/// Tool schemas for Lead: base + task/epic management tools + submit_decision finalize tool.
/// task_comment_add and task_transition are excluded — submit_decision drives transitions.
pub(crate) fn tool_schemas_lead() -> Vec<serde_json::Value> {
    tool_schemas_pm()
}

/// Tool schemas for PM (Lead): base + task/epic management tools + submit_decision finalize tool.
/// task_comment_add and task_transition are excluded — submit_decision drives transitions.
pub(crate) fn tool_schemas_pm() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in [
        serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
        serde_json::to_value(tool_task_update()).expect("serialize tool_task_update"),
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_task_blocked_list()).expect("serialize tool_task_blocked_list"),
        serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
        serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
        serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        serde_json::to_value(tool_request_architect()).expect("serialize tool_request_architect"),
        serde_json::to_value(crate::roles::finalize::tool_submit_decision())
            .expect("serialize tool_submit_decision"),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Planner: base + task/epic management tools + submit_grooming finalize tool.
/// task_comment_add is excluded — submit_grooming captures session output.
pub(crate) fn tool_schemas_planner() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in [
        serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
        serde_json::to_value(tool_task_update()).expect("serialize tool_task_update"),
        serde_json::to_value(tool_task_transition()).expect("serialize tool_task_transition"),
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_task_blocked_list()).expect("serialize tool_task_blocked_list"),
        serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
        serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
        serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        serde_json::to_value(crate::roles::finalize::tool_submit_grooming())
            .expect("serialize tool_submit_grooming"),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Architect: read-only tools, task/epic management, submit_work,
/// and agent effectiveness tools (role_metrics, memory_build_context, role_amend_prompt).
/// Does not include write/edit/apply_patch. The Architect diagnoses and directs but does not write code.
pub(crate) fn tool_schemas_architect() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in [
        serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
        serde_json::to_value(tool_task_comment_add()).expect("serialize tool_task_comment_add"),
        serde_json::to_value(tool_task_transition()).expect("serialize tool_task_transition"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_task_blocked_list()).expect("serialize tool_task_blocked_list"),
        serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
        serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
        serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        serde_json::to_value(tool_memory_build_context())
            .expect("serialize tool_memory_build_context"),
        serde_json::to_value(tool_role_metrics()).expect("serialize tool_role_metrics"),
        serde_json::to_value(tool_role_amend_prompt()).expect("serialize tool_role_amend_prompt"),
        serde_json::to_value(crate::roles::finalize::tool_submit_work())
            .expect("serialize tool_submit_work"),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Public entry point for the Djinn-native reply loop to call a tool by name.
///
/// `arguments` should be the `input` field from a `ContentBlock::ToolUse`
/// converted to an `Option<Map>`:
///
/// ```rust,ignore
/// let args = match input {
///     Value::Object(map) => Some(map),
///     _ => None,
/// };
/// ```
pub(crate) async fn call_tool(
    state: &AgentContext,
    name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let synthetic = serde_json::json!({ "name": name, "arguments": arguments });
    dispatch_tool_call(state, &synthetic, worktree_path, None, session_task_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentType;
    use crate::test_helpers::create_test_db;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn floor_char_boundary_ascii() {
        assert_eq!(floor_char_boundary("hello", 3), 3);
    }

    #[test]
    fn floor_char_boundary_multibyte_interior() {
        // '─' (U+2500) is 3 bytes: E2 94 80
        let s = "─";
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 3);
    }

    #[test]
    fn floor_char_boundary_emoji() {
        // '🔥' is 4 bytes
        let s = "🔥x";
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 0);
        assert_eq!(floor_char_boundary(s, 4), 4);
        assert_eq!(floor_char_boundary(s, 5), 5);
    }

    #[test]
    fn floor_char_boundary_beyond_len() {
        assert_eq!(floor_char_boundary("hi", 100), 2);
    }

    #[test]
    fn floor_char_boundary_zero() {
        assert_eq!(floor_char_boundary("hello", 0), 0);
    }

    #[tokio::test]
    async fn write_rejects_symlink_escape_outside_worktree() {
        use tempfile::tempdir;

        let worktree = tempdir().expect("temp worktree");
        let outside = tempdir().expect("outside dir");
        let link = worktree.path().join("escape-link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

        let args = Some(
            serde_json::json!({"path":"escape-link/pwned.txt","content":"owned"})
                .as_object()
                .expect("obj")
                .clone(),
        );

        let state =
            crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
        let result = call_write(&state, &args, worktree.path()).await;
        assert!(result.is_err());
        let err = result.err().unwrap_or_default();
        assert!(err.contains("outside worktree"));
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[test]
    fn worker_cannot_use_pm_only_tool() {
        // submit_decision is PM-only (ADR-036: finalize tools are role-specific).
        assert!(!is_tool_allowed_for_agent(
            AgentType::Worker,
            "submit_decision"
        ));
        assert!(is_tool_allowed_for_agent(
            AgentType::Lead,
            "submit_decision"
        ));
        // task_transition is not in the PM tool set (removed by ADR-036).
        assert!(!is_tool_allowed_for_agent(
            AgentType::Lead,
            "task_transition"
        ));
    }

    #[test]
    fn shell_timeout_defaults_and_minimum() {
        fn resolve_timeout(t: Option<u64>) -> u64 {
            t.unwrap_or(120_000).clamp(1000, 600_000)
        }
        assert_eq!(resolve_timeout(None), 120_000);
        assert_eq!(resolve_timeout(Some(0)), 1000);
        assert_eq!(resolve_timeout(Some(1_200_000)), 600_000);
    }

    #[test]
    fn tool_schemas_include_role_specific_tools() {
        fn schema_names(schemas: Vec<serde_json::Value>) -> Vec<String> {
            schemas
                .into_iter()
                .filter_map(|v| {
                    v.get("name")
                        .and_then(|n| n.as_str())
                        .map(ToString::to_string)
                })
                .collect()
        }

        let worker = schema_names(tool_schemas_worker());
        assert!(worker.iter().any(|n| n == "shell"));
        assert!(worker.iter().any(|n| n == "write"));
        assert!(worker.iter().any(|n| n == "edit"));
        assert!(worker.iter().any(|n| n == "submit_work"));
        assert!(!worker.iter().any(|n| n == "task_comment_add"));

        let reviewer = schema_names(tool_schemas_reviewer());
        assert!(reviewer.iter().any(|n| n == "submit_review"));
        assert!(!reviewer.iter().any(|n| n == "task_update_ac"));
        assert!(!reviewer.iter().any(|n| n == "task_comment_add"));

        let lead = schema_names(tool_schemas_lead());
        assert!(lead.iter().any(|n| n == "task_create"));
        assert!(lead.iter().any(|n| n == "submit_decision"));
        assert!(!lead.iter().any(|n| n == "task_transition"));
        assert!(!lead.iter().any(|n| n == "task_comment_add"));

        let planner = schema_names(tool_schemas_planner());
        assert!(planner.iter().any(|n| n == "task_create"));
        assert!(planner.iter().any(|n| n == "task_transition"));
        assert!(planner.iter().any(|n| n == "submit_grooming"));
        assert!(!planner.iter().any(|n| n == "task_comment_add"));

        let architect = schema_names(tool_schemas_architect());
        assert!(architect.iter().any(|n| n == "shell"));
        assert!(architect.iter().any(|n| n == "read"));
        assert!(architect.iter().any(|n| n == "task_create"));
        assert!(architect.iter().any(|n| n == "task_comment_add"));
        assert!(architect.iter().any(|n| n == "task_transition"));
        assert!(architect.iter().any(|n| n == "task_kill_session"));
        assert!(architect.iter().any(|n| n == "submit_work"));
        // Architect must NOT have code-writing tools.
        assert!(!architect.iter().any(|n| n == "write"));
        assert!(!architect.iter().any(|n| n == "edit"));
        assert!(!architect.iter().any(|n| n == "apply_patch"));
    }

    #[test]
    fn ensure_path_within_worktree_accepts_in_tree_and_rejects_traversal() {
        use tempfile::tempdir;

        let worktree = tempdir().expect("temp worktree");
        let nested = worktree.path().join("nested");
        std::fs::create_dir_all(&nested).expect("create nested");
        let in_tree = nested.join("file.txt");
        ensure_path_within_worktree(&in_tree, worktree.path()).expect("in-tree path should pass");

        let traversal = worktree.path().join("..").join("..").join("escape.txt");
        let err = ensure_path_within_worktree(&traversal, worktree.path())
            .expect_err("traversal should be rejected");
        assert!(err.contains("outside worktree"));
    }

    #[test]
    fn ensure_path_within_worktree_rejects_symlink_escape() {
        use tempfile::tempdir;

        let worktree = tempdir().expect("temp worktree");
        let outside = tempdir().expect("outside");
        let link = worktree.path().join("escape-link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

        let escaped = link.join("leak.txt");
        let err = ensure_path_within_worktree(&escaped, worktree.path())
            .expect_err("symlink escape should be rejected");
        assert!(err.contains("outside worktree"));
    }

    #[test]
    fn is_tool_allowed_for_schemas_handles_empty_and_invalid_entries() {
        assert!(!is_tool_allowed_for_schemas(&[], "shell"));

        let schemas = vec![
            serde_json::json!({}),
            serde_json::json!({"name": null}),
            serde_json::json!({"name": 42}),
            serde_json::json!({"name": "shell"}),
        ];
        assert!(is_tool_allowed_for_schemas(&schemas, "shell"));
        assert!(!is_tool_allowed_for_schemas(&schemas, "read"));
    }

    #[test]
    fn resolve_path_handles_relative_absolute_and_normalization() {
        let base = Path::new("/tmp/worktree");

        let relative = resolve_path("src/main.rs", base);
        assert_eq!(relative, PathBuf::from("/tmp/worktree/src/main.rs"));

        let absolute = resolve_path("/etc/hosts", base);
        assert_eq!(absolute, PathBuf::from("/etc/hosts"));

        let normalized = resolve_path("./src/../Cargo.toml", base);
        assert_eq!(normalized, PathBuf::from("/tmp/worktree/Cargo.toml"));
    }

    fn tool_names(schemas: &[serde_json::Value]) -> Vec<&str> {
        schemas
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
            .collect()
    }

    #[test]
    fn snapshot_worker_tool_names() {
        let schemas = tool_schemas_worker();
        let names = tool_names(&schemas);
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("worker_tool_names", names);
        });
    }

    #[test]
    fn snapshot_worker_tool_schemas() {
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("worker_tool_schemas", tool_schemas_worker());
        });
    }

    #[test]
    fn snapshot_reviewer_tool_names() {
        let schemas = tool_schemas_reviewer();
        let names = tool_names(&schemas);
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("reviewer_tool_names", names);
        });
    }

    #[test]
    fn snapshot_reviewer_tool_schemas() {
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("reviewer_tool_schemas", tool_schemas_reviewer());
        });
    }

    #[test]
    fn snapshot_lead_tool_names() {
        let schemas = tool_schemas_lead();
        let names = tool_names(&schemas);
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("lead_tool_names", names);
        });
    }

    #[test]
    fn snapshot_lead_tool_schemas() {
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("lead_tool_schemas", tool_schemas_lead());
        });
    }

    #[test]
    fn snapshot_planner_tool_names() {
        let schemas = tool_schemas_planner();
        let names = tool_names(&schemas);
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("planner_tool_names", names);
        });
    }

    #[test]
    fn snapshot_planner_tool_schemas() {
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("planner_tool_schemas", tool_schemas_planner());
        });
    }

    #[test]
    fn snapshot_architect_tool_names() {
        let schemas = tool_schemas_architect();
        let names = tool_names(&schemas);
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("architect_tool_names", names);
        });
    }

    #[test]
    fn snapshot_architect_tool_schemas() {
        let mut settings = insta::Settings::clone_current();
        settings.set_snapshot_path("../snapshots");
        settings.bind(|| {
            insta::assert_json_snapshot!("architect_tool_schemas", tool_schemas_architect());
        });
    }
}
