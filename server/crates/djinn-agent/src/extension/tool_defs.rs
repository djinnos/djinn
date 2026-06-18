use rmcp::model::Tool as RmcpTool;
use rmcp::object;

use super::shared_schemas::{self, ToolSafetyAnnotations};

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
        "Planner-owned, evidence-based amendment path for machine-managed learned_prompt updates. Use only after agent-effectiveness evidence (agent_metrics, repeated reviewer/lead feedback, or repeated task failures) shows a stable specialist-agent pattern; this is audited in learned_prompt_history and is not a substitute for human/project system_prompt_extensions. Only specialist worker/reviewer agents are eligible; default roles and non-worker/reviewer roles must not be amended. Append concise observed-pattern + behavioral-correction text; provide metrics_snapshot when available so the evaluator can compare before/after outcomes."
            .to_string(),
        object!({
            "type": "object",
            "required": ["agent_id", "amendment"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "agent_id": {"type": "string", "description": "Specialist worker/reviewer agent UUID or name to amend; defaults and non-worker/reviewer roles are rejected"},
                "amendment": {"type": "string", "description": "Concise observed-pattern and behavioral-correction text to append to the machine-managed learned_prompt"},
                "metrics_snapshot": {"type": "string", "description": "Optional JSON string of current agent_metrics or equivalent evidence for the audit history record; Planner should provide it when available"}
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

pub(crate) fn tool_code_search() -> RmcpTool {
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

pub(crate) fn tool_skill_read() -> RmcpTool {
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

pub(crate) fn tool_output_view() -> RmcpTool {
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

pub(crate) fn tool_output_grep() -> RmcpTool {
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
        "Query the SCIP-built repository dependency graph. Prefer `uid` as the stable \
         exact node input; fall back to `name` + `file_path` + `kind` when a UID is \
         unavailable; ambiguous names return ranked candidates. Agent-boundary traversal \
         triage controls: `limit`, `offset`, `pageLimit`, `summaryOnly`, `byDepthCounts`. \
         Partial pages and capped summaries are triage views; absence from a page or \
         summary is NOT evidence a node/edge/pair is absent from the full graph. `workspaces` \
         lists available-workspace slugs plus name/node_count/commit_sha/warmed_at/status; \
         unknown workspace degradation includes `workspace_hint`. STALENESS: pass \
         `current_head` (your current git commit SHA) when known — every successful \
         response will then include an additive `graph_staleness` object comparing your \
         commit against the cached graph blob. The flag is serve-stale-with-warning only: \
         it NEVER blocks the query and NEVER triggers re-warming; you decide whether to \
         trust the result, retry later, or ask the user to re-warm. WHEN TO USE: capabilities \
         = cheap discovery of supported ops/params/defaults before spending graph budget; \
         query_subgraph = ask a natural-language question in `query` and get a token-budgeted \
         focused subgraph with narrowing hints; search = find candidate files/symbols by \
         substring when you do not know a key; describe = inspect one exact file/symbol key; \
         neighbors = direct callers/callees/imports around a known key; impact = bounded \
         transitive dependents for what-breaks-if-this-changes; context = compact bucketed \
         incoming/outgoing context for a symbol; ranked/cycles/orphans/path/edges = broader \
         structural queries; symbols_at/diff_touches/detect_changes = line range to touched \
         symbols; status/metrics_at/snapshot/workspaces = graph health/introspection; \
         api_surface/dead_symbols/deprecated_callers/route_map/shape_check/api_impact/flow = \
         public surface and route/flow health; boundary_check/blast_radius/touches_hot_path = \
         change-impact analysis; hotspots/cochange/churn/coupling_hubs = git-coupling × PageRank \
         centrality; complexity/refactor_candidates = budget-conscious discovery of \
         risky/refactorable code. AFTER THIS: after capabilities call the chosen op with only \
         required fields; after query_subgraph inspect returned seeds/budget/truncation and \
         retry with context_filter, file_filter, edge_filters, max_depth/max_seeds, or \
         token_budget if too broad; after search pass a returned key to describe, neighbors, \
         context, or impact; after describe call neighbors or context for relationships, or \
         impact for dependents; after neighbors call describe on important nodes or impact to \
         expand beyond direct edges; after impact call blast_radius or boundary_check for \
         review/test planning; after context call describe/impact on highlighted neighbors."
            .to_string(),
        object!({
            "type": "object",
            "required": ["operation", "project"],
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "neighbors", "ranked", "impact", "implementations",
                        "search", "query_subgraph", "cycles", "orphans", "path", "edges",
                        "symbols_at", "diff_touches", "detect_changes",
                        "describe", "context", "api_surface", "boundary_check",
                        "blast_radius", "hotspots", "complexity", "refactor_candidates", "cochange",
                        "churn", "coupling_hubs", "metrics_at", "dead_symbols",
                        "deprecated_callers", "touches_hot_path", "status",
                        "snapshot", "workspaces", "capabilities"
                    ],
                    "description": "Graph query to perform. Start with capabilities for cheap supported-op discovery; use query_subgraph with natural-language query for a focused token-budgeted subgraph; use search when you need a key, describe/context for one key, neighbors for direct edges, and impact for bounded transitive dependents."
                },
                "project": {
                    "type": "string",
                    "description": "Project slug (owner/repo) or UUID. The handler resolves it to the server-managed clone path."
                },
                "workspace": {
                    "type": "string",
                    "description": "Optional workspace slug. Empty string is treated as omitted. Use operation=workspaces to enumerate available slugs and metadata (name/node_count/commit_sha/warmed_at/status). Known workspaces hard-scope listing/bounded ops (ranked/orphans/snapshot/api_surface) and scope only seed/endpoint resolution for traversal ops (impact/path/touches_hot_path/blast_radius) so cross-workspace edges remain visible. Unknown non-empty slugs return unscoped results with workspace_hint available-workspace candidates where supported."
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
                    "description": "Node kind filter for ranked/search/cycles/orphans and query_subgraph seed/traversal narrowing"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Max results (ranked/search/orphans/edges) or max traversal depth (impact)"
                },
                "query": {
                    "type": "string",
                    "description": "Substring query for search, or the required natural-language question for operation='query_subgraph'. After search, feed returned keys to describe, neighbors, context, or impact."
                },
                "context_filter": {
                    "type": "string",
                    "description": "Optional coarse subsystem/API/type/concern substring for operation='query_subgraph'. Use returned narrowing_hints to choose values when the first response is too broad."
                },
                "current_head": {
                    "type": "string",
                    "description": "Optional caller HEAD / git commit SHA. When provided, every successful response includes an additive `graph_staleness` object comparing this commit against the cached graph blob's pinned commit (is_stale=true when they differ). The flag is serve-stale-with-warning only: the query is never blocked and graph re-warming is never auto-triggered. Omit when the caller does not know its current commit. `caller_commit` and `currentHead` are accepted as aliases."
                },
                "file_filter": {
                    "type": "string",
                    "description": "Optional repository-relative path/file substring for operation='query_subgraph'. Narrows seed selection and traversal; file_glob is also accepted as a compatibility alias."
                },
                "edge_filters": {
                    "type": "array",
                    "description": "Optional explicit edge kinds for operation='query_subgraph' traversal (for example calls, imports, returns, reads, writes, implements, extends). Omit to let the planner infer useful kinds from query; edge_kind is accepted as a single-kind alias.",
                    "items": {"type": "string"}
                },
                "token_budget": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Approximate response token budget for operation='query_subgraph'. Omit for backend default (~2000); positive values clamp to 1024..=32000 and the response reports budget/truncation state plus narrowing_hints."
                },
                "max_seeds": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum seed count for operation='query_subgraph'. Omit for backend default (~6); positive values clamp to 1..=32. Seed debug metadata explains which seeds were selected."
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
                    "enum": [
                        "pagerank", "in_degree", "out_degree", "total_degree",
                        "cognitive", "cyclomatic", "nloc", "max_nesting", "param_count"
                    ],
                    "description": "Sort key. For ranked: pagerank|in_degree|out_degree|total_degree (default pagerank). For complexity: cognitive|cyclomatic|nloc|max_nesting|param_count (default cognitive)."
                },
                "target": {
                    "type": "string",
                    "enum": ["functions", "files"],
                    "description": "Target tier for complexity (default functions). 'files' aggregates by file_path."
                },
                "tests": {
                    "type": "string",
                    "enum": ["include", "exclude", "only"],
                    "description": "Test-file filter for snapshot: include (default, whole graph), exclude (drop test files/symbols), only (test nodes only). Uses the canonical is_test classification (file-path convention OR SCIP Test role)."
                },
                "group_by": {
                    "type": "string",
                    "enum": ["file"],
                    "description": "Group impact/neighbors results by file"
                },
                "max_depth": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Maximum depth for path, or operation='query_subgraph' traversal depth from selected seeds (0 = seed nodes only; clamped to 0..=8 for query_subgraph)"
                },
                "edge_kind": {
                    "type": "string",
                    "description": "Edge-kind filter for edges; for query_subgraph, a single-kind compatibility alias when edge_filters is omitted"
                },
                "file": {
                    "type": "string",
                    "description": "Repository-relative file path (required for symbols_at)"
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-indexed inclusive start line (required for symbols_at)"
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-indexed inclusive end line for symbols_at (defaults to start_line)"
                },
                "changed_ranges": {
                    "type": "array",
                    "description": "List of changed line ranges for diff_touches (parsed from `git diff --unified=0 base..head`)",
                    "items": {
                        "type": "object",
                        "required": ["file", "start_line"],
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "Repository-relative path of the changed file"
                            },
                            "start_line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-indexed inclusive first line of the hunk"
                            },
                            "end_line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-indexed inclusive last line of the hunk (defaults to start_line)"
                            }
                        }
                    }
                },
                "module_glob": {
                    "type": "string",
                    "description": "File-path glob restricting api_surface to a subset of symbols"
                },
                "confidence": {
                    "type": "string",
                    "enum": ["high", "med", "low"],
                    "description": "Confidence tier for dead_symbols (default high)"
                },
                "window_days": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Churn look-back window in days for hotspots (default 90, clamped to 365)"
                },
                "file_glob": {
                    "type": "string",
                    "description": "File-path glob restricting hotspots to a subset of files; for query_subgraph, compatibility alias for file_filter"
                },
                "rules": {
                    "type": "array",
                    "description": "Boundary rules for boundary_check. Every submitted rule is treated as a forbidden edge; matching edges are reported as violations.",
                    "items": {
                        "type": "object",
                        "required": ["from_glob", "to_glob"],
                        "properties": {
                            "from_glob": {
                                "type": "string",
                                "description": "Source path glob"
                            },
                            "to_glob": {
                                "type": "string",
                                "description": "Destination path glob"
                            }
                        }
                    }
                },
                "seed_entries": {
                    "type": "array",
                    "description": "Entry-point SCIP symbol keys for touches_hot_path",
                    "items": {"type": "string"}
                },
                "seed_sinks": {
                    "type": "array",
                    "description": "Sink SCIP symbol keys for touches_hot_path",
                    "items": {"type": "string"}
                },
                "symbols": {
                    "type": "array",
                    "description": "Queried symbol keys for touches_hot_path",
                    "items": {"type": "string"}
                },
                "mode": {
                    "type": "string",
                    "enum": ["name", "hybrid"],
                    "description": "Search mode for the `search` op (PR B4). `name` runs the canonical-graph name index only (fast path); `hybrid` blends lexical + semantic + structural via RRF k=60 and stamps each hit with a `match_kind` tag for debug surfaces. Default reads `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` (env var; defaults to `name`)."
                }
            }
        }),
    )
}

pub(crate) fn tool_pr_review_context() -> RmcpTool {
    RmcpTool::new(
        "pr_review_context".to_string(),
        "Given a PR's changed line ranges (parsed from `git diff --unified=0 base..head`), assemble the base-graph signals that matter for review in one call: touched symbols with fan-in/out, blast radius, hotspot overlap, touched cycles, deprecated-caller hits, hot-path overlap, and architecture-boundary violations. Base-graph-only — does NOT build a head graph, detect newly-introduced cycles, or parse the diff text for removed-API detection. Every list is capped (defaults: touched_symbols=100, blast_radius=50, hotspot_overlap=20, touched_cycles=20, touched_boundary_violations=50, touched_deprecated=20, hot_path_overlap=20)."
            .to_string(),
        object!({
            "type": "object",
            "required": ["project", "changed_ranges"],
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Project slug (owner/repo) or UUID. The handler resolves it to the server-managed clone path."
                },
                "changed_ranges": {
                    "type": "array",
                    "description": "List of changed line ranges parsed from `git diff --unified=0 base..head`",
                    "items": {
                        "type": "object",
                        "required": ["file", "start_line"],
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "Repository-relative path of the changed file"
                            },
                            "start_line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-indexed inclusive first line of the hunk"
                            },
                            "end_line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "1-indexed inclusive last line of the hunk (defaults to start_line)"
                            }
                        }
                    }
                },
                "seed_entries": {
                    "type": "array",
                    "description": "Entry-point SCIP keys for hot-path overlap (optional)",
                    "items": {"type": "string"}
                },
                "seed_sinks": {
                    "type": "array",
                    "description": "Sink SCIP keys for hot-path overlap (optional)",
                    "items": {"type": "string"}
                },
                "boundary_rules": {
                    "type": "array",
                    "description": "Architecture boundary rules; when empty, boundary analysis is skipped",
                    "items": {
                        "type": "object",
                        "required": ["from_glob", "to_glob", "forbidden"],
                        "properties": {
                            "from_glob": {"type": "string"},
                            "to_glob": {"type": "string"},
                            "forbidden": {"type": "boolean"}
                        }
                    }
                },
                "hotspots_window_days": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Churn look-back window for hotspot overlap (default 90)"
                },
                "caps": {
                    "type": "object",
                    "description": "Per-list result caps; missing fields use defaults",
                    "properties": {
                        "touched_symbols": {"type": "integer", "minimum": 0},
                        "blast_radius": {"type": "integer", "minimum": 0},
                        "hotspot_overlap": {"type": "integer", "minimum": 0},
                        "touched_cycles": {"type": "integer", "minimum": 0},
                        "touched_boundary_violations": {"type": "integer", "minimum": 0},
                        "touched_deprecated": {"type": "integer", "minimum": 0},
                        "hot_path_overlap": {"type": "integer", "minimum": 0}
                    }
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
pub(crate) fn tool_schemas_worker() -> Vec<serde_json::Value> {
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
    tool_values.push(serialize_tool(tool_request_lead(), mutation()));
    tool_values.push(serialize_tool(
        crate::roles::finalize::tool_submit_work(),
        mutation(),
    ));
    tool_values
}

/// Tool schemas for Reviewer: base + submit_review finalize tool.
/// task_update_ac is excluded — submit_review sets AC atomically.
pub(crate) fn tool_schemas_reviewer() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serialize_tool(
        shared_schemas::tool_memory_build_context(),
        read_only(),
    ));
    tool_values.push(serialize_tool(
        crate::roles::finalize::tool_submit_review(),
        mutation(),
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
        serialize_tool(tool_task_delete_branch(), destructive()),
        serialize_tool(tool_task_archive_activity(), destructive()),
        serialize_tool(tool_task_reset_counters(), idempotent_destructive()),
        serialize_tool(tool_task_kill_session(), destructive()),
        serialize_tool(tool_request_planner(), mutation()),
        serialize_tool(crate::roles::finalize::tool_submit_decision(), mutation()),
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
pub(crate) fn tool_schemas_planner() -> Vec<serde_json::Value> {
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
        serialize_tool(tool_role_amend_prompt(), destructive()),
        serialize_tool(crate::roles::finalize::tool_submit_grooming(), mutation()),
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
        // Per ADR-051 §1, `role_amend_prompt` has moved to the Planner —
        // agent-effectiveness amendment is a Planner action, not a consultant
        // action. Architect keeps `role_metrics` (read) and `role_create`
        // (structural proposal) but cannot mutate existing learned_prompts.
        serialize_tool(crate::roles::finalize::tool_submit_work(), mutation()),
    ] {
        tool_values.push(value);
    }
    tool_values
}
