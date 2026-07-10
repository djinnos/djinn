use rmcp::model::Tool as RmcpTool;
use std::collections::BTreeSet;

use rmcp::object;

use crate::shared_schemas::{self, ToolSafetyAnnotations};

fn serialize_tool(tool: RmcpTool, annotations: ToolSafetyAnnotations) -> serde_json::Value {
    shared_schemas::serialize_tool_schema(tool, annotations)
}

fn read_only() -> ToolSafetyAnnotations {
    ToolSafetyAnnotations::read_only()
}

fn open_world_read_only() -> ToolSafetyAnnotations {
    ToolSafetyAnnotations::open_world_read_only()
}

fn mutation() -> ToolSafetyAnnotations {
    ToolSafetyAnnotations::mutation()
}

fn idempotent_mutation() -> ToolSafetyAnnotations {
    ToolSafetyAnnotations::idempotent_mutation()
}

fn destructive() -> ToolSafetyAnnotations {
    ToolSafetyAnnotations::destructive()
}

fn idempotent_destructive() -> ToolSafetyAnnotations {
    ToolSafetyAnnotations::idempotent_destructive()
}

/// [HISTORICAL-COMPAT] Retained for the `request_lead` drain window after
/// epic 10qg cut-over.  Production worker/reviewer surfaces no longer
/// advertise this tool; it exists so that stale sessions dispatched before
/// the cutover can still call it and be routed to Planner via the
/// deprecated compatibility handler.  Tests reference this definition to
/// verify the tool name is absent from active role schemas.
pub fn tool_request_lead() -> RmcpTool {
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

pub fn tool_request_planner() -> RmcpTool {
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

pub fn tool_shell() -> RmcpTool {
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

pub fn tool_read() -> RmcpTool {
    RmcpTool::new(
        "read".to_string(),
        "Read a file with line numbers and pagination. Rejects binary files. Pass `project` (UUID or owner/repo slug) to read a file from ANOTHER registered repo — served read-only from its default branch (no checkout); omit it to read your task worktree.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1 },
                "project": { "type": "string", "description": "Other registered project to read from (UUID or owner/repo slug). Omit for your own task repo." }
            },
            "required": ["file_path"]
        }),
    )
}

pub fn tool_code_search() -> RmcpTool {
    RmcpTool::new(
        "code_search".to_string(),
        "Search code across registered repos with `git grep` (basic regex). Pass `project` (UUID or owner/repo slug) to search one repo, or omit it (or pass \"*\") to search ALL registered projects at once — e.g. find every caller of a gRPC service org-wide. Served from each repo's default branch (no checkout). For your own task repo's working tree (your uncommitted changes), use shell grep instead.".to_string(),
        object!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string", "description": "Pattern (basic regex, like git grep)"},
                "project": {"type": "string", "description": "Limit to one project (UUID or owner/repo slug). Omit or \"*\" = all registered projects."},
                "path": {"type": "string", "description": "Optional pathspec to scope the search (e.g. crates/ or *.rs)"},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive match"},
                "max_results": {"type": "integer", "description": "Max hits per project (default 100)"}
            }
        }),
    )
}

pub fn tool_skill_read() -> RmcpTool {
    RmcpTool::new(
        "skill_read".to_string(),
        "Load the full content of an assigned skill by name. Under progressive \
         skill disclosure the system prompt lists each non-required skill's name \
         and description only; call this to fetch the complete skill body on \
         demand. Errors if the name is not an assigned skill."
            .to_string(),
        object!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {"type": "string", "description": "Name of the skill to load (as shown in the Available Skills section)"}
            }
        }),
    )
}

pub fn tool_write() -> RmcpTool {
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

pub fn tool_edit() -> RmcpTool {
    RmcpTool::new(
        "edit".to_string(),
        "Edit a file by replacing text. Provide path, old_text, and new_text. Rescues common whitespace/indentation drift automatically; fails on ambiguous matches or safety-guard rejections.".to_string(),
        object!({
            "type": "object",
            "required": ["path", "old_text", "new_text"],
            "properties": {
                "path": {"type": "string", "description": "Absolute or worktree-relative file path"},
                "old_text": {"type": "string", "description": "Text to find and replace (may differ from file in whitespace, indentation, or encoding; the matcher rescues common drift automatically)"},
                "new_text": {"type": "string", "description": "Replacement text"}
            }
        }),
    )
}

pub fn tool_task_delete_branch() -> RmcpTool {
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

pub fn tool_task_archive_activity() -> RmcpTool {
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

pub fn tool_task_reset_counters() -> RmcpTool {
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

pub fn tool_task_kill_session() -> RmcpTool {
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

pub fn tool_ci_job_log() -> RmcpTool {
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

pub fn tool_output_view() -> RmcpTool {
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

pub fn tool_output_grep() -> RmcpTool {
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

pub fn tool_apply_patch() -> RmcpTool {
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

pub fn tool_lsp() -> RmcpTool {
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

pub use crate::tool_defs_code_graph::{tool_code_graph, tool_pr_review_context};

pub fn tool_github_search() -> RmcpTool {
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

// `github_fetch_file` retired: reading our own repos goes through
// `read(project=…)` (mirror-backed, full local file), and `github_search`
// remains for external-ecosystem snippets.

// ─── Schema aggregation ────────────────────────────────────────────────────

fn base_tool_schemas() -> Vec<serde_json::Value> {
    let mut tool_values = shared_schemas::shared_base_tool_schemas();
    tool_values.push(serialize_tool(tool_shell(), destructive()));
    tool_values.push(serialize_tool(tool_read(), read_only()));
    tool_values.push(serialize_tool(tool_code_search(), open_world_read_only()));
    tool_values.push(serialize_tool(tool_skill_read(), read_only()));
    tool_values.push(serialize_tool(tool_lsp(), read_only()));
    // NOTE: `tool_code_graph()` is intentionally NOT in the base schema set.
    // Per ADR-050, the code-graph tool is exclusive to the Architect (autonomous form)
    // and the Chat surface (interactive form). Worker, reviewer, planner, and lead do not
    // see it. The architect's role-specific schema function appends it directly.
    tool_values.push(serialize_tool(tool_ci_job_log(), read_only()));
    tool_values.push(serialize_tool(tool_github_search(), open_world_read_only()));
    tool_values.push(serialize_tool(tool_output_view(), read_only()));
    tool_values.push(serialize_tool(tool_output_grep(), read_only()));
    tool_values
}

/// Tool schemas for Worker: base + file-editing tools.
pub fn tool_schemas_worker() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(tool_write(), destructive()));
    tool_values.push(serialize_tool(tool_edit(), destructive()));
    tool_values.push(serialize_tool(tool_apply_patch(), destructive()));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    // Workers may deliberately record durable knowledge (decisions, patterns,
    // pitfalls hit during the task) — complements the automatic post-session
    // extraction. Previously memory writes were Architect-only.
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_write(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_edit(),
        mutation(),
    ));
    tool_values.push(serialize_tool(tool_request_planner(), mutation()));
    tool_values.push(serialize_tool(
        crate::finalize_tools::tool_submit_work(),
        mutation(),
    ));
    tool_values
}

/// Tool schemas for Reviewer: base + submit_review finalize tool + request_planner escalation.
/// task_update_ac is excluded — submit_review sets AC atomically.
pub fn tool_schemas_reviewer() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    // request_planner is the intended planner-escalation path for reviewers.
    tool_values.push(serialize_tool(tool_request_planner(), mutation()));
    tool_values.push(serialize_tool(
        crate::finalize_tools::tool_submit_review(),
        mutation(),
    ));
    tool_values
}

/// Tool schemas for Lead: base + task/epic management tools + submit_decision finalize tool.
/// task_comment_add and task_transition are excluded — submit_decision drives transitions.
pub fn tool_schemas_lead() -> Vec<serde_json::Value> {
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
        serialize_tool(tool_task_delete_branch(), destructive()),
        serialize_tool(tool_task_archive_activity(), destructive()),
        serialize_tool(tool_task_reset_counters(), idempotent_destructive()),
        serialize_tool(tool_task_kill_session(), destructive()),
        serialize_tool(crate::finalize_tools::tool_submit_decision(), mutation()),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Planner: base + task/epic management tools + memory/role
/// management tools (per ADR-051 §1) + submit_grooming
/// finalize tool.
///
/// The Planner now runs in two modes: (a) per-epic decomposition (the legacy
/// mode) and (b) board-health maintenance (migrated from Architect). The tool
/// surface is the union of both needs. `code_graph` remains Architect-only
/// (per ADR-050) because deep structural analysis is an Architect spike, not
/// a Planner responsibility.
pub fn tool_schemas_planner() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(tool_write(), destructive()));
    tool_values.push(serialize_tool(tool_edit(), destructive()));
    for value in shared_schemas::shared_lead_tool_schemas() {
        tool_values.push(value);
    }
    // Proposal decomposition (Mode D): create epics across the proposal's
    // targets, sequence them with dependencies, and read the proposal + sibling
    // repos.
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_create(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_show(),
        read_only(),
    ));
    // Proposal closeout (Mode E): reconcile AC met-flags as epics land, then
    // mark the building proposal done once every criterion is satisfied.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_ac_set(),
        idempotent_mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_ac_amend(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_complete(),
        idempotent_destructive(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_reconcile_obsolete_epic(),
        idempotent_destructive(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_blockers_list(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_blocked_list(),
        read_only(),
    ));
    // Sibling-repo survey for Mode D uses the base `read(project=…)` and
    // `code_search` tools (mirror-backed, full local repos) — no github_* needed.
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_transition(),
        mutation(),
    ));
    // task_comment_add was previously excluded for planners (submit_grooming
    // captured output), but the Planner needs to leave diagnostic comments on
    // stuck tasks.
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_comment_add(),
        mutation(),
    ));
    // Memory-health and knowledge-graph tools used by board maintenance
    // (sections "Memory Health Review" and "Contradiction and Low-Confidence
    // Review", formerly the patrol prompt).
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_health(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_extracted_audit(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_broken_links(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_orphans(),
        read_only(),
    ));
    // The Planner may curate the knowledge base directly (annotate/fix notes during
    // the Memory Health Review), so expose write + edit alongside the read tools.
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_write(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_edit(),
        mutation(),
    ));
    // Agent effectiveness review tools (migrated from Architect §10 per ADR-051
    // ADR-051 ownership migration).
    tool_values.push(serialize_tool(
        shared_schemas::tool_role_metrics(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_role_create(),
        destructive(),
    ));
    for value in [
        serialize_tool(tool_task_delete_branch(), destructive()),
        serialize_tool(tool_task_archive_activity(), destructive()),
        serialize_tool(tool_task_reset_counters(), idempotent_destructive()),
        serialize_tool(tool_task_kill_session(), destructive()),
        serialize_tool(crate::finalize_tools::tool_submit_grooming(), mutation()),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Architect: read-only tools, task/epic management, submit_work,
/// and agent effectiveness tools (role_metrics, memory_build_context).
/// Does not include write/edit/apply_patch. The Architect diagnoses and directs but does not write code.
pub fn tool_schemas_architect() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    // Per ADR-050, the Architect (and only the Architect among agent roles) gets the
    // `code_graph` tool. Inserted at the position the base set used to occupy so the
    // schema ordering matches the historical layout.
    let lsp_pos = tool_values
        .iter()
        .position(|v| v.get("name").and_then(|n| n.as_str()) == Some("lsp"))
        .map(|i| i + 1)
        .unwrap_or(tool_values.len());
    tool_values.insert(
        lsp_pos,
        serialize_tool(tool_code_graph(), open_world_read_only()),
    );
    // Phase 3: the `pr_review_context` meta-tool rides the same Architect-only
    // access contract as `code_graph` — it's a base-graph analysis surface
    // aimed at PR review.
    tool_values.insert(
        lsp_pos + 1,
        serialize_tool(tool_pr_review_context(), read_only()),
    );
    for value in shared_schemas::shared_lead_tool_schemas() {
        tool_values.push(value);
    }
    // Per ADR-050 §2, parity contract: chat exposes `epic_create`; the Architect must too.
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_create(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_transition(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_comment_add(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_health(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_extracted_audit(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_broken_links(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_orphans(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_role_metrics(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_role_create(),
        destructive(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_write(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_edit(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_move(),
        mutation(),
    ));
    for value in [
        serialize_tool(tool_task_delete_branch(), destructive()),
        serialize_tool(tool_task_archive_activity(), destructive()),
        serialize_tool(tool_task_reset_counters(), idempotent_destructive()),
        serialize_tool(tool_task_kill_session(), destructive()),
        // Architect keeps `role_metrics` (read) and `role_create`
        // (structural proposal) but has no mutation tool for role prompts.
        serialize_tool(crate::finalize_tools::tool_submit_work(), mutation()),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Advocate: base + write/edit + memory + proposal tools +
/// proposal block enrichment tools + submit_work finalize tool.
///
/// The Advocate authors/revises proposal specifications in the tribunal
/// refinement workflow (k9zw). It has write access for proposal enrichment,
/// block-catalog enrichment tools for progressive MDX refinement, and memory
/// tools for design rationale.
pub fn tool_schemas_advocate() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(tool_write(), destructive()));
    tool_values.push(serialize_tool(tool_edit(), destructive()));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_write(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_edit(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_show(),
        read_only(),
    ));
    // Read the debate trail to see exactly which adversary objections this
    // revision must address. The advocate only READS the trail — it does not
    // rebut or resolve. The Judge adjudicates and resolves objections.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_list(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_update(),
        mutation(),
    ));
    // Silent AC update (no feedback comment). `proposal_ac_amend` is
    // deliberately NOT given to the advocate: its per-amendment "reason" is
    // persisted as an AI feedback comment, which spams the proposal during
    // refinement.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_ac_set(),
        idempotent_mutation(),
    ));
    // Block-catalog enrichment tools for progressive MDX refinement (k9zw).
    // These are default-but-optional enrichment; failure to enrich must not
    // fail the deterministic DoR evaluator.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_block_patch(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_get_block_catalog(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_blocks(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        crate::finalize_tools::tool_submit_work(),
        mutation(),
    ));
    tool_values
}

/// Tool schemas for Adversary: base + memory + proposal read tools +
/// task_comment_add + submit_review finalize tool.
///
/// The Adversary produces falsifiable blocking/non-blocking objections to
/// proposal specifications (k9zw). Read-only by design — it challenges but
/// does not modify proposals.
pub fn tool_schemas_adversary() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_show(),
        read_only(),
    ));
    // Read the debate trail to avoid re-raising objections from prior rounds.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_list(),
        read_only(),
    ));
    // The Adversary files each objection here — the ONLY channel the refinement
    // loop reads. Objections placed in `submit_review` are NOT read by the loop.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_append(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_comment_add(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        crate::finalize_tools::tool_submit_review(),
        mutation(),
    ));
    tool_values
}

/// Tool schemas for Judge: base + memory + proposal read tools +
/// proposal_debate_append (verdict) + task_comment_add + submit_decision.
///
/// The Judge adjudicates proposal readiness after the Adversary is dry (k9zw).
/// It records its verdict via `proposal_debate_append` (the channel the loop
/// reads) and does not otherwise modify proposals.
pub fn tool_schemas_judge() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_show(),
        read_only(),
    ));
    // Read the full debate trail to verify each blocking objection was
    // resolved or rebutted before ruling.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_list(),
        read_only(),
    ));
    // The Judge adjudicates resolution: for each blocking objection the
    // current revision now satisfies, it calls `proposal_debate_resolve` to
    // clear it from the readiness gate's unresolved-blocking set. This is the
    // tribunal's resolution authority (the advocate only revises the spec).
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_resolve(),
        idempotent_mutation(),
    ));
    // The Judge records its readiness verdict here (kind="verdict") — the ONLY
    // channel the refinement loop reads. A verdict in `submit_decision` text is
    // NOT read by the loop.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_append(),
        mutation(),
    ));
    // The Judge can demand a read-only evidence spike when in-session research
    // is insufficient to resolve a load-bearing feasibility claim.
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_refinement_demand_evidence(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_comment_add(),
        mutation(),
    ));
    tool_values.push(serialize_tool(
        crate::finalize_tools::tool_submit_decision(),
        mutation(),
    ));
    tool_values
}

/// Tool schemas for an evidence-spike session: read-only investigation tools
/// plus the `submit_work` finalize path for evidence findings handoff.
///
/// Evidence-spike tasks are created by the Judge demand-evidence tool
/// (`proposal_refinement_demand_evidence` in epic `6tjy`) and carry the
/// `refinement-evidence` + `read-only` labels.  They run under the
/// Architect role but with a severely restricted, read-only/fail-closed
/// tool surface.
///
/// **Allowed:**
/// - Base read-only tools (read, code_search, lsp, ci_job_log, github_search,
///   output_view, output_grep, skill_read — but NOT shell).
/// - Task/epic/memory read-only inspection tools (task_show, task_list,
///   task_activity_list, memory_read, memory_search, memory_list,
///   task_blocked_list, epic_show, epic_tasks).
/// - Memory health/context tools (memory_build_context, memory_health,
///   memory_extracted_audit, memory_broken_links, memory_orphans).
/// - Architect-only read tools (code_graph, pr_review_context).
/// - `submit_work` as the finalize tool for evidence findings handoff.
///
/// **Denied (not included):**
/// - `shell` (destructive — may mutate files/system).
/// - `write`, `edit`, `apply_patch` (file mutation).
/// - `task_create`, `task_update`, `task_transition`, `task_comment_add`
///   (task mutation — except the finalize path).
/// - `epic_create`, `epic_update`, `epic_close` (epic mutation).
/// - `memory_write`, `memory_edit`, `memory_move` (memory mutation).
/// - `role_create` (destructive agent mutation).
/// - `task_delete_branch`, `task_archive_activity`, `task_reset_counters`,
///   `task_kill_session` (destructive task admin).
/// - `request_planner` (mutation escalation).
///   (`request_lead` is a [HISTORICAL-COMPAT] definition only — not part of
///   the active evidence-spike tool surface.)
/// - Any proposal/debate mutation tools.
pub fn tool_schemas_evidence_spike() -> Vec<serde_json::Value> {
    let mut tool_values = Vec::new();

    // ── Read-only inspection tools (from shared_base_tool_schemas) ─────
    // These are task/activity and memory read-only inspection tools.
    for value in shared_schemas::shared_base_tool_schemas() {
        tool_values.push(value);
    }

    // ── Read-only file/code intelligence tools (from base, NO shell) ──
    tool_values.push(serialize_tool(tool_read(), read_only()));
    tool_values.push(serialize_tool(tool_code_search(), open_world_read_only()));
    tool_values.push(serialize_tool(tool_skill_read(), read_only()));
    tool_values.push(serialize_tool(tool_lsp(), read_only()));
    tool_values.push(serialize_tool(tool_ci_job_log(), read_only()));
    tool_values.push(serialize_tool(tool_github_search(), open_world_read_only()));
    tool_values.push(serialize_tool(tool_output_view(), read_only()));
    tool_values.push(serialize_tool(tool_output_grep(), read_only()));

    // ── Architect-only read tools ─────────────────────────────────────
    tool_values.push(serialize_tool(tool_code_graph(), open_world_read_only()));
    tool_values.push(serialize_tool(tool_pr_review_context(), read_only()));

    // ── Read-only task/epic inspection (from shared_lead, read-only) ──
    tool_values.push(serialize_tool(
        shared_schemas::tool_task_blocked_list(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_show(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_epic_tasks(),
        read_only(),
    ));

    // ── Read-only memory health/context tools ─────────────────────────
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_health(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_extracted_audit(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_broken_links(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_orphans(),
        read_only(),
    ));

    // ── Read-only proposal/debate inspection (for context) ────────────
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_show(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        shared_schemas::tool_proposal_debate_list(),
        read_only(),
    ));

    // ── Finalize tool: evidence findings submission path ──────────────
    // This is the ONLY mutation-capable tool allowed — it is the spike's
    // own completion/handoff path via submit_work.
    tool_values.push(serialize_tool(
        crate::finalize_tools::tool_submit_work(),
        mutation(),
    ));

    tool_values
}

/// Canonical allowlist of tool names permitted in the evidence-spike
/// read-only profile.
///
/// Derived from [`tool_schemas_evidence_spike`] to guarantee the allowlist
/// and the schema surface cannot drift.  Callers that need to gate dispatch
/// at runtime (fail-closed) should use this set rather than re-parsing
/// schemas.
///
/// This is the single source of truth for the evidence-spike allowlist;
/// schema generation and dispatch enforcement both reference this function
/// so they cannot drift apart.
pub fn evidence_spike_tool_names() -> BTreeSet<String> {
    tool_schemas_evidence_spike()
        .iter()
        .filter_map(|schema| {
            schema
                .get("name")
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect()
}
