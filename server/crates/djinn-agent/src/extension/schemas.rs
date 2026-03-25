use rmcp::model::Tool as RmcpTool;
use rmcp::object;

mod shared_schemas;

use shared_schemas::{
    shared_architect_tool_schemas, shared_base_tool_schemas, shared_planner_tool_schemas,
    shared_lead_tool_schemas,
};

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
        "Escalate a task to the Architect role when implementation is blocked on codebase analysis, root-cause investigation, or cross-cutting technical design. This is for strategic technical intervention rather than lead triage."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "reason": {"type": "string", "description": "Why Architect intervention is needed (e.g. root cause unclear, design blocked, need codebase-wide analysis)"}
            }
        }),
    )
}

fn tool_role_amend_prompt() -> RmcpTool {
    RmcpTool::new(
        "agent_amend_prompt".to_string(),
        "Append a learned amendment to a role prompt using observed failures or improvements."
            .to_string(),
        object!({
            "type": "object",
            "required": ["role", "amendment"],
            "properties": {
                "role": {"type": "string", "description": "Role name to amend"},
                "amendment": {"type": "string", "description": "Instruction text to append to the role prompt"},
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
            "required": ["file_path"],
            "properties": {
                "file_path": {"type": "string", "description": "Absolute or worktree-relative file path"},
                "offset": {"type": "integer", "minimum": 0},
                "limit": {"type": "integer", "minimum": 1}
            }
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


fn tool_task_delete_branch() -> RmcpTool {
    RmcpTool::new(
        "task_delete_branch".to_string(),
        "Delete the task worktree and branch so a fresh implementation can restart from target branch state.".to_string(),
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

fn tool_apply_patch() -> RmcpTool {
    RmcpTool::new(
        "apply_patch".to_string(),
        concat!(
            "Apply a patch to one or more files using a custom LLM-friendly format. ",
            "Uses content-based context matching (not line numbers). Format:\n\n",
            "*** Begin Patch\n",
            "*** Update File: path/to/file.rs\n",
            "@@ context_line_from_file\n",
            " context line (unchanged)\n",
            "-old line to remove\n",
            "+new line to add\n",
            " context line (unchanged)\n\n",
            "*** Add File: path/to/new_file.rs\n",
            "+line 1\n",
            "+line 2\n\n",
            "*** Delete File: path/to/old_file.rs\n",
            "*** End Patch\n\n",
            "Rules: ' ' prefix = context (must match file), '-' = delete, '+' = add. ",
            "The @@ line text is searched in the file to locate each chunk. ",
            "Multiple @@ chunks per file are allowed. ",
            "Files being updated or deleted must be read first.",
        )
        .to_string(),
        object!({
            "type": "object",
            "required": ["patch"],
            "properties": {
                "patch": {"type": "string", "description": "Patch content in the custom format (see tool description)"}
            }
        }),
    )
}

fn tool_lsp() -> RmcpTool {
    RmcpTool::new(
        "lsp".to_string(),
        "Query the Language Server Protocol for code navigation. Operations: hover (type info at position), definition (go to definition), references (find all references), symbols (list document symbols). Line and character are 1-based.".to_string(),
        object!({
            "type": "object",
            "required": ["operation", "file_path"],
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["hover", "definition", "references", "symbols"],
                    "description": "LSP operation to perform"
                },
                "file_path": {
                    "type": "string",
                    "description": "Absolute or worktree-relative file path"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number (required for hover, definition, references)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based column number (required for hover, definition, references)"
                }
            }
        }),
    )
}

pub(crate) fn base_tool_schemas() -> Vec<serde_json::Value> {
    let mut tool_values = shared_base_tool_schemas();
    tool_values.push(serde_json::to_value(tool_shell()).expect("serialize tool_shell"));
    tool_values.push(serde_json::to_value(tool_read()).expect("serialize tool_read"));
    tool_values.push(serde_json::to_value(tool_lsp()).expect("serialize tool_lsp"));
    tool_values.push(serde_json::to_value(tool_output_view()).expect("serialize tool_output_view"));
    tool_values.push(serde_json::to_value(tool_output_grep()).expect("serialize tool_output_grep"));
    tool_values
}

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

pub(crate) fn tool_schemas_reviewer() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(
        serde_json::to_value(crate::roles::finalize::tool_submit_review())
            .expect("serialize tool_submit_review"),
    );
    tool_values
}

pub(crate) fn tool_schemas_lead() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in shared_lead_tool_schemas().into_iter().chain([
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_request_architect()).expect("serialize tool_request_architect"),
        serde_json::to_value(crate::roles::finalize::tool_submit_decision())
            .expect("serialize tool_submit_decision"),
    ]) {
        tool_values.push(value);
    }
    tool_values
}

pub(crate) fn tool_schemas_planner() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in shared_planner_tool_schemas().into_iter().chain([
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(crate::roles::finalize::tool_submit_grooming())
            .expect("serialize tool_submit_grooming"),
    ]) {
        tool_values.push(value);
    }
    tool_values
}

pub(crate) fn tool_schemas_architect() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in shared_architect_tool_schemas().into_iter().chain([
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_role_amend_prompt()).expect("serialize tool_role_amend_prompt"),
        serde_json::to_value(crate::roles::finalize::tool_submit_work())
            .expect("serialize tool_submit_work"),
    ]) {
        tool_values.push(value);
    }
    tool_values
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn tool_names(schemas: &[serde_json::Value]) -> Vec<&str> {
        schemas
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
            .collect()
    }

    #[test]
    fn tool_schemas_include_role_specific_tools() {
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
        assert!(architect.iter().any(|n| n == "task_update"));
        assert!(architect.iter().any(|n| n == "task_comment_add"));
        assert!(architect.iter().any(|n| n == "task_transition"));
        assert!(architect.iter().any(|n| n == "task_delete_branch"));
        assert!(architect.iter().any(|n| n == "task_archive_activity"));
        assert!(architect.iter().any(|n| n == "task_reset_counters"));
        assert!(architect.iter().any(|n| n == "task_kill_session"));
        assert!(architect.iter().any(|n| n == "submit_work"));
        assert!(!architect.iter().any(|n| n == "write"));
        assert!(!architect.iter().any(|n| n == "edit"));
        assert!(!architect.iter().any(|n| n == "apply_patch"));
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
