use rmcp::model::Tool as RmcpTool;
use rmcp::object;

use super::shared_schemas;

fn serialize_tool(tool: RmcpTool, concurrent_safe: bool) -> serde_json::Value {
    shared_schemas::serialize_tool_schema(tool, concurrent_safe)
}

pub(super) fn tool_request_lead() -> RmcpTool {
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

pub(super) fn tool_request_planner() -> RmcpTool {
    RmcpTool::new(
        "request_planner".to_string(),
        "Escalate to the Planner when the task requires board-level intervention beyond per-task Lead resolution. Use when the task is mis-shaped, duplicates other work, needs to be split or merged, or has failed multiple Lead interventions. The Planner owns the board and decides whether to reshape the work, dedupe it, or — if the issue requires deeper code-structural reasoning — dispatch an Architect spike. Adds a comment and dispatches the Planner. Your session should end after this call."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short_id"},
                "reason": {"type": "string", "description": "Why Planner escalation is needed (e.g. task mis-shaped, duplicates other work, needs splitting, repeated Lead failures with no clear next step)"}
            }
        }),
    )
}

pub(super) fn tool_role_amend_prompt() -> RmcpTool {
    RmcpTool::new(
        "agent_amend_prompt".to_string(),
        "Append a prompt amendment to a specialist agent role's learned_prompt. The amendment is appended after existing content (never replacing it) and logged to learned_prompt_history. Only applicable to specialist roles (worker, reviewer base_role). Do NOT use on architect, lead, or planner roles.".to_string(),
        object!({
            "type": "object",
            "required": ["agent_id", "amendment"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "agent_id": {"type": "string", "description": "Agent UUID or name to amend"},
                "amendment": {"type": "string", "description": "Amendment text to append to learned_prompt"},
                "metrics_snapshot": {"type": "string", "description": "JSON string of current metrics for the history record"}
            }
        }),
    )
}

pub(crate) fn tool_shell() -> RmcpTool {
    RmcpTool::new(
        "shell".to_string(),
        "Execute shell commands in the task worktree. Commands always run from the worktree root."
            .to_string(),
        object!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 120000)"}
            }
        }),
    )
}

pub(crate) fn tool_read() -> RmcpTool {
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

pub(super) fn tool_write() -> RmcpTool {
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

pub(super) fn tool_edit() -> RmcpTool {
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

pub(super) fn tool_task_delete_branch() -> RmcpTool {
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

pub(super) fn tool_task_archive_activity() -> RmcpTool {
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

pub(super) fn tool_task_reset_counters() -> RmcpTool {
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

pub(super) fn tool_task_kill_session() -> RmcpTool {
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

pub(super) fn tool_ci_job_log() -> RmcpTool {
    RmcpTool::new(
        "ci_job_log".to_string(),
        "Fetch the full log for a GitHub Actions CI job. When CI fails, the activity log \
         tells you the job_id. Call this tool to see the actual error output. Optionally \
         filter to a specific failed step name. If the output is large, use output_view \
         or output_grep to navigate it."
            .to_string(),
        object!({
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": {"type": "integer", "description": "The GitHub Actions job ID from the CI failure activity"},
                "step": {"type": "string", "description": "Optional step name to filter the log to (e.g. 'Tests')"}
            }
        }),
    )
}

pub(super) fn tool_output_view() -> RmcpTool {
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

pub(super) fn tool_output_grep() -> RmcpTool {
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

pub(super) fn tool_apply_patch() -> RmcpTool {
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

pub(crate) fn tool_lsp() -> RmcpTool {
    RmcpTool::new(
        "lsp".to_string(),
        "Query the Language Server Protocol for code navigation. Operations: hover (type info at position), definition (go to definition), references (find all references), symbols (list document symbols with optional depth/kind/name filtering). Line and character are 1-based for non-symbol operations.".to_string(),
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
                    "description": "1-based column number (required for hover, definition, references when symbol is omitted)"
                },
                "symbol": {
                    "type": "string",
                    "description": "Optional symbol name path for hover, definition, or references as an alternative to line+character"
                },
                "depth": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Maximum nesting depth for operation='symbols'. 0 = top-level only; omitted = unlimited"
                },
                "kind": {
                    "type": "string",
                    "description": "Comma-separated symbol kind filter for operation='symbols' (e.g. function,method,struct,class,interface,enum,variable,constant,module,field,property,constructor,type_parameter)"
                },
                "name_filter": {
                    "type": "string",
                    "description": "Case-insensitive substring filter applied to symbol names and name paths for operation='symbols'"
                }
            }
        }),
    )
}

pub(crate) fn tool_code_graph() -> RmcpTool {
    RmcpTool::new(
        "code_graph".to_string(),
        "Query the repository dependency graph built from SCIP indexer output. Operations: \
         neighbors (edges in/out of a node; group_by=file for per-file rollup), \
         ranked (top nodes by PageRank or degree; sort_by pagerank|in_degree|out_degree|total_degree), \
         impact (transitive dependents; group_by=file for per-file rollup), \
         implementations (find implementors of a trait/interface symbol), \
         search (name-based symbol lookup via query substring), \
         cycles (strongly-connected components of size >= min_size), \
         orphans (zero-incoming-reference nodes filtered by visibility public|private|any), \
         path (shortest dependency path from→to), \
         edges (enumerate edges matching from_glob→to_glob), \
         diff (what changed since the previous canonical graph — only since=previous supported), \
         describe (symbol signature/documentation without an LSP round trip).".to_string(),
        object!({
            "type": "object",
            "required": ["operation", "project_path"],
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "neighbors", "ranked", "impact", "implementations",
                        "search", "cycles", "orphans", "path", "edges", "diff", "describe"
                    ],
                    "description": "Graph query to perform"
                },
                "project_path": {
                    "type": "string",
                    "description": "Absolute path to project root"
                },
                "key": {
                    "type": "string",
                    "description": "Node key: file path or SCIP symbol string (required for neighbors, impact, implementations, describe)"
                },
                "direction": {
                    "type": "string",
                    "enum": ["incoming", "outgoing"],
                    "description": "Edge direction filter for neighbors (omit for both)"
                },
                "kind_filter": {
                    "type": "string",
                    "enum": ["file", "symbol"],
                    "description": "Node kind filter for ranked/search/cycles/orphans"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Max results (ranked/search/orphans/edges) or max traversal depth (impact)"
                },
                "query": {
                    "type": "string",
                    "description": "Substring query for search"
                },
                "from": {
                    "type": "string",
                    "description": "Source node for path"
                },
                "to": {
                    "type": "string",
                    "description": "Destination node for path"
                },
                "from_glob": {
                    "type": "string",
                    "description": "Source path glob for edges"
                },
                "to_glob": {
                    "type": "string",
                    "description": "Destination path glob for edges"
                },
                "since": {
                    "type": "string",
                    "description": "Diff base selector for diff (currently only 'previous')"
                },
                "min_size": {
                    "type": "integer",
                    "minimum": 2,
                    "description": "Minimum SCC size for cycles (default 2)"
                },
                "visibility": {
                    "type": "string",
                    "enum": ["public", "private", "any"],
                    "description": "Visibility filter for orphans (default any)"
                },
                "sort_by": {
                    "type": "string",
                    "enum": ["pagerank", "in_degree", "out_degree", "total_degree"],
                    "description": "Sort key for ranked (default pagerank)"
                },
                "group_by": {
                    "type": "string",
                    "enum": ["file"],
                    "description": "Group impact/neighbors results by file"
                },
                "max_depth": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum depth for path"
                },
                "edge_kind": {
                    "type": "string",
                    "description": "Edge-kind filter for edges"
                }
            }
        }),
    )
}

pub(crate) fn tool_github_search() -> RmcpTool {
    RmcpTool::new(
        "github_search".to_string(),
        "Search GitHub code across public repositories using the GitHub Code Search API. \
         Returns compact, navigable matches with snippets, file paths, URLs, and metadata. \
         Each result has a result_id for reference. Use github_fetch_file to inspect the \
         full file of a promising result."
            .to_string(),
        object!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. Supports GitHub code search syntax."
                },
                "language": {
                    "type": "string",
                    "description": "Programming language filter (e.g. \"Rust\", \"Python\", \"TypeScript\")"
                },
                "repo": {
                    "type": "string",
                    "description": "Repository filter in \"owner/repo\" format (e.g. \"tokio-rs/tokio\")"
                },
                "path": {
                    "type": "string",
                    "description": "Path filter to search within specific directories (e.g. \"src/\")"
                }
            }
        }),
    )
}

pub(crate) fn tool_github_fetch_file() -> RmcpTool {
    RmcpTool::new(
        "github_fetch_file".to_string(),
        "Fetch the full contents of a file from a public GitHub repository. \
         Use after github_search to inspect a promising result. Supports \
         optional start_line/end_line for reading specific sections of large files."
            .to_string(),
        object!({
            "type": "object",
            "required": ["repo", "path"],
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repository in \"owner/repo\" format (e.g. \"tokio-rs/tokio\")"
                },
                "path": {
                    "type": "string",
                    "description": "File path within the repository (e.g. \"src/lib.rs\")"
                },
                "ref": {
                    "type": "string",
                    "description": "Branch, tag, or commit SHA (default: default branch)"
                },
                "start_line": {
                    "type": "integer",
                    "description": "First line to return (1-based, inclusive)"
                },
                "end_line": {
                    "type": "integer",
                    "description": "Last line to return (1-based, inclusive)"
                }
            }
        }),
    )
}

// ─── Schema aggregation ────────────────────────────────────────────────────

fn base_tool_schemas() -> Vec<serde_json::Value> {
    let mut tool_values = shared_schemas::shared_base_tool_schemas();
    tool_values.push(serialize_tool(tool_shell(), false));
    tool_values.push(serialize_tool(tool_read(), true));
    tool_values.push(serialize_tool(tool_lsp(), true));
    // NOTE: `tool_code_graph()` is intentionally NOT in the base schema set.
    // Per ADR-050, the code-graph tool is exclusive to the Architect (autonomous patrol form)
    // and the Chat surface (interactive form). Worker, reviewer, planner, and lead do not
    // see it. The architect's role-specific schema function appends it directly.
    tool_values.push(serialize_tool(tool_ci_job_log(), true));
    tool_values.push(serialize_tool(tool_github_search(), true));
    tool_values.push(serialize_tool(tool_github_fetch_file(), true));
    tool_values.push(serialize_tool(tool_output_view(), true));
    tool_values.push(serialize_tool(tool_output_grep(), true));
    tool_values
}

/// Tool schemas for Worker and Resolver: base + file-editing tools.
pub(crate) fn tool_schemas_worker() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(tool_write(), false));
    tool_values.push(serialize_tool(tool_edit(), false));
    tool_values.push(serialize_tool(tool_apply_patch(), false));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_write(), false));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_edit(), false));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        false,
    ));
    tool_values.push(serialize_tool(tool_request_lead(), false));
    tool_values.push(serialize_tool(
        crate::roles::finalize::tool_submit_work(),
        false,
    ));
    tool_values
}

/// Tool schemas for Reviewer: base + submit_review finalize tool.
/// task_update_ac is excluded — submit_review sets AC atomically.
pub(crate) fn tool_schemas_reviewer() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        false,
    ));
    tool_values.push(serialize_tool(
        crate::roles::finalize::tool_submit_review(),
        false,
    ));
    tool_values
}

/// Tool schemas for Lead: base + task/epic management tools + submit_decision finalize tool.
/// task_comment_add and task_transition are excluded — submit_decision drives transitions.
pub(crate) fn tool_schemas_lead() -> Vec<serde_json::Value> {
    tool_schemas_lead_inner()
}

/// Tool schemas for Lead: base + task/epic management tools + submit_decision finalize tool.
/// task_comment_add and task_transition are excluded — submit_decision drives transitions.
fn tool_schemas_lead_inner() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in shared_schemas::shared_lead_tool_schemas() {
        tool_values.push(value);
    }
    for value in [
        serialize_tool(tool_task_delete_branch(), false),
        serialize_tool(tool_task_archive_activity(), false),
        serialize_tool(tool_task_reset_counters(), false),
        serialize_tool(tool_task_kill_session(), false),
        serialize_tool(tool_request_planner(), false),
        serialize_tool(crate::roles::finalize::tool_submit_decision(), false),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Planner: base + task/epic management tools + memory/role
/// management tools (needed by patrol mode per ADR-051 §1) + submit_grooming
/// finalize tool.
///
/// The Planner now runs in two modes: (a) per-epic decomposition (the legacy
/// mode) and (b) board-health patrol (migrated from Architect). The tool
/// surface is the union of both needs. `code_graph` remains Architect-only
/// (per ADR-050) because deep structural analysis is an Architect spike, not
/// a patrol responsibility.
pub(crate) fn tool_schemas_planner() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in shared_schemas::shared_lead_tool_schemas() {
        tool_values.push(value);
    }
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_transition(),
        false,
    ));
    // task_comment_add was previously excluded for planners (submit_grooming
    // captured output), but patrol mode needs to leave diagnostic comments on
    // stuck tasks.
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_comment_add(),
        false,
    ));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_write(), false));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_edit(), false));
    // Memory-health and knowledge-graph tools used by the patrol workflow
    // (sections "Memory Health Review" and "Contradiction and Low-Confidence
    // Review" in the patrol prompt).
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        true,
    ));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_health(), true));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_broken_links(),
        true,
    ));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_orphans(), true));
    // Agent effectiveness review tools (migrated from Architect §10 per ADR-051
    // patrol ownership migration).
    tool_values.push(serialize_tool(shared_schemas::tool_role_metrics(), true));
    tool_values.push(serialize_tool(shared_schemas::tool_role_create(), false));
    for value in [
        serialize_tool(tool_task_delete_branch(), false),
        serialize_tool(tool_task_archive_activity(), false),
        serialize_tool(tool_task_reset_counters(), false),
        serialize_tool(tool_task_kill_session(), false),
        serialize_tool(tool_role_amend_prompt(), false),
        serialize_tool(crate::roles::finalize::tool_submit_grooming(), false),
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
    // Per ADR-050, the Architect (and only the Architect among agent roles) gets the
    // `code_graph` tool. Inserted at the position the base set used to occupy so the
    // schema ordering matches the historical layout.
    let lsp_pos = tool_values
        .iter()
        .position(|v| v.get("name").and_then(|n| n.as_str()) == Some("lsp"))
        .map(|i| i + 1)
        .unwrap_or(tool_values.len());
    tool_values.insert(lsp_pos, serialize_tool(tool_code_graph(), true));
    for value in shared_schemas::shared_lead_tool_schemas() {
        tool_values.push(value);
    }
    // Per ADR-050 §2, parity contract: chat exposes `epic_create`; the Architect must too.
    tool_values.push(serialize_tool(shared_schemas::tool_epic_create(), false));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_transition(),
        false,
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_comment_add(),
        false,
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        true,
    ));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_health(), true));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_broken_links(),
        true,
    ));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_orphans(), true));
    tool_values.push(serialize_tool(shared_schemas::tool_role_metrics(), true));
    tool_values.push(serialize_tool(shared_schemas::tool_role_create(), false));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_write(), false));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_edit(), false));
    tool_values.push(serialize_tool(shared_schemas::tool_memory_move(), false));
    for value in [
        serialize_tool(tool_task_delete_branch(), false),
        serialize_tool(tool_task_archive_activity(), false),
        serialize_tool(tool_task_reset_counters(), false),
        serialize_tool(tool_task_kill_session(), false),
        // Per ADR-051 §1, `role_amend_prompt` has moved to the Planner —
        // agent-effectiveness amendment is a patrol action, not a consultant
        // action. Architect keeps `role_metrics` (read) and `role_create`
        // (structural proposal) but cannot mutate existing learned_prompts.
        serialize_tool(crate::roles::finalize::tool_submit_work(), false),
    ] {
        tool_values.push(value);
    }
    tool_values
}
