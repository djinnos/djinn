use rmcp::model::Tool as RmcpTool;
use rmcp::object;

pub(crate) fn shared_base_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        serde_json::to_value(tool_task_show()).expect("serialize tool_task_show"),
        serde_json::to_value(tool_task_list()).expect("serialize tool_task_list"),
        serde_json::to_value(tool_task_activity_list()).expect("serialize tool_task_activity_list"),
        serde_json::to_value(tool_memory_read()).expect("serialize tool_memory_read"),
        serde_json::to_value(tool_memory_search()).expect("serialize tool_memory_search"),
        serde_json::to_value(tool_memory_list()).expect("serialize tool_memory_list"),
    ]
}

pub(crate) fn shared_lead_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
        serde_json::to_value(tool_task_update()).expect("serialize tool_task_update"),
        serde_json::to_value(tool_task_blocked_list()).expect("serialize tool_task_blocked_list"),
        serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
        serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
        serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        serde_json::to_value(tool_epic_close()).expect("serialize tool_epic_close"),
    ]
}

pub(crate) fn tool_epic_close() -> RmcpTool {
    RmcpTool::new(
        "epic_close".to_string(),
        "Close an epic. Use when all work is complete and no further waves are needed.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"}
            }
        }),
    )
}

pub(crate) fn tool_epic_show() -> RmcpTool {
    RmcpTool::new(
        "epic_show".to_string(),
        "Show details for an epic by UUID or short ID.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"}
            }
        }),
    )
}

pub(crate) fn tool_epic_update() -> RmcpTool {
    RmcpTool::new(
        "epic_update".to_string(),
        "Update epic fields (title/description) and accept memory ref delta args for planner workflows.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "status": {"type": "string"},
                "memory_refs_add": {"type": "array", "items": {"type": "string"}},
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}}
            }
        }),
    )
}

pub(crate) fn tool_epic_tasks() -> RmcpTool {
    RmcpTool::new(
        "epic_tasks".to_string(),
        "List tasks for an epic with pagination.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"},
                "limit": {"type": "integer"},
                "offset": {"type": "integer"}
            }
        }),
    )
}

pub(crate) fn tool_task_list() -> RmcpTool {
    RmcpTool::new(
        "task_list".to_string(),
        "List tasks with optional filters and pagination.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "status": {"type": "string"},
                "issue_type": {"type": "string"},
                "priority": {"type": "integer"},
                "text": {"type": "string", "description": "Free-text search in title/description"},
                "label": {"type": "string"},
                "parent": {"type": "string", "description": "Epic ID to filter by"},
                "sort": {"type": "string"},
                "limit": {"type": "integer"},
                "offset": {"type": "integer"}
            }
        }),
    )
}

pub(crate) fn tool_task_blocked_list() -> RmcpTool {
    RmcpTool::new(
        "task_blocked_list".to_string(),
        "List tasks that are blocked by the given task. Use before decomposing to check downstream dependents.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

pub(crate) fn tool_task_activity_list() -> RmcpTool {
    RmcpTool::new(
        "task_activity_list".to_string(),
        "Query a task's activity log with optional filters. Returns comments, status transitions, verification results, and other events. Use to inspect Lead guidance, reviewer feedback, or verification history.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "event_type": {"type": "string", "description": "Filter by event type: comment, status_changed, commands_run, merge_conflict, task_review_start"},
                "actor_role": {"type": "string", "description": "Filter by actor: lead, reviewer, worker, verification, system"},
                "limit": {"type": "integer", "description": "Max entries to return (default 30, max 50)"}
            }
        }),
    )
}

pub(crate) fn tool_task_show() -> RmcpTool {
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

pub(crate) fn tool_memory_read() -> RmcpTool {
    RmcpTool::new(
        "memory_read".to_string(),
        "Read a note by permalink or title.".to_string(),
        object!({
            "type": "object",
            "required": ["identifier"],
            "properties": {
                "identifier": {"type": "string", "description": "Permalink or title"}
            }
        }),
    )
}

pub(crate) fn tool_memory_search() -> RmcpTool {
    RmcpTool::new(
        "memory_search".to_string(),
        "Search notes in project memory.".to_string(),
        object!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string"},
                "folder": {"type": "string"},
                "type": {"type": "string"},
                "task_id": {"type": "string", "description": "Task ID for affinity scoring; defaults to the current session task"},
                "limit": {"type": "integer"}
            }
        }),
    )
}

pub(crate) fn tool_memory_list() -> RmcpTool {
    RmcpTool::new(
        "memory_list".to_string(),
        "List notes in project memory. Returns compact summaries without full content.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "folder": {"type": "string", "description": "Filter by folder (e.g. \"decisions\")"},
                "type": {"type": "string", "description": "Filter by note type (e.g. \"adr\", \"reference\", \"research\")"},
                "depth": {"type": "integer", "description": "Depth control: 0 = unlimited, 1 = exact folder (default), N = N levels"}
            }
        }),
    )
}

pub(crate) fn tool_memory_write() -> RmcpTool {
    RmcpTool::new(
        "memory_write".to_string(),
        "Create or update a note. Type is required and determines storage folder (adr->decisions/, pattern->patterns/, case->cases/, pitfall->pitfalls/, research->research/, requirement->requirements/, reference->reference/, design->design/, tech_spike->research/technical, session->research/sessions). Singleton types (brief, roadmap) write a fixed file — one per project. Use [[wikilinks]] in content to connect notes.".to_string(),
        object!({
            "type": "object",
            "required": ["title", "content", "type"],
            "properties": {
                "title": {"type": "string", "description": "Note title"},
                "content": {"type": "string", "description": "Markdown content of the note. Use [[wikilinks]] to connect to other notes."},
                "type": {"type": "string", "description": "Note type: adr, pattern, case, pitfall, research, requirement, reference, design, tech_spike, session, brief (singleton), roadmap (singleton)"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional tags for categorisation"}
            }
        }),
    )
}

pub(crate) fn tool_memory_edit() -> RmcpTool {
    RmcpTool::new(
        "memory_edit".to_string(),
        "Edit an existing note. Operations: \"append\" (add to end), \"prepend\" (add after frontmatter), \"find_replace\" (exact text replacement, requires find_text), \"replace_section\" (replace content under a markdown heading, requires section).".to_string(),
        object!({
            "type": "object",
            "required": ["identifier", "operation", "content"],
            "properties": {
                "identifier": {"type": "string", "description": "Note permalink or title"},
                "operation": {"type": "string", "description": "Edit operation: append, prepend, find_replace, replace_section"},
                "content": {"type": "string", "description": "New content to insert or replace with"},
                "find_text": {"type": "string", "description": "Required for find_replace: exact text to search for"},
                "section": {"type": "string", "description": "Required for replace_section: heading text identifying the section"},
                "type": {"type": "string", "description": "If provided and different from current type, move the note to the new type's folder"}
            }
        }),
    )
}

pub(crate) fn tool_memory_build_context() -> RmcpTool {
    RmcpTool::new(
        "memory_build_context".to_string(),
        "Build a curated memory context pack for a task or query by combining note retrieval and ranking. Use this before deep analysis to gather relevant project history and decisions.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task ID to gather related memory for; defaults to current session task when omitted"},
                "query": {"type": "string", "description": "Optional free-text query to bias retrieval"},
                "limit": {"type": "integer", "description": "Maximum notes to include (default 8)"},
                "min_confidence": {"type": "number", "description": "Minimum confidence threshold for related notes (default 0.1). Notes below this are excluded."}
            }
        }),
    )
}

pub(crate) fn tool_memory_health() -> RmcpTool {
    RmcpTool::new(
        "memory_health".to_string(),
        "Returns aggregate health report: total notes, broken link count, orphan note count, stale notes by folder.".to_string(),
        object!({
            "type": "object",
            "properties": {}
        }),
    )
}

pub(crate) fn tool_memory_broken_links() -> RmcpTool {
    RmcpTool::new(
        "memory_broken_links".to_string(),
        "Lists all broken wikilinks with source context (permalink, title, raw text, target permalink).".to_string(),
        object!({
            "type": "object",
            "properties": {
                "folder": {"type": "string", "description": "Optional folder filter (e.g. \"decisions\")"}
            }
        }),
    )
}

pub(crate) fn tool_memory_orphans() -> RmcpTool {
    RmcpTool::new(
        "memory_orphans".to_string(),
        "Lists notes with zero inbound links. Excludes catalogs and singletons (brief, roadmap)."
            .to_string(),
        object!({
            "type": "object",
            "properties": {
                "folder": {"type": "string", "description": "Optional folder filter (e.g. \"pitfalls\")"}
            }
        }),
    )
}

pub(crate) fn tool_role_metrics() -> RmcpTool {
    RmcpTool::new(
        "agent_metrics".to_string(),
        "Show execution quality metrics for a role to support prompt tuning and intervention decisions.".to_string(),
        object!({
            "type": "object",
            "required": ["role"],
            "properties": {
                "role": {"type": "string", "description": "Role name (worker, reviewer, lead, planner, architect)"}
            }
        }),
    )
}

pub(crate) fn tool_role_create() -> RmcpTool {
    RmcpTool::new(
        "agent_create".to_string(),
        "Create a new specialist agent extending a base role (worker or reviewer). Use when existing agents lack capabilities for a specific domain."
            .to_string(),
        object!({
            "type": "object",
            "required": ["name", "base_role"],
            "properties": {
                "name": {"type": "string", "description": "Unique agent name within the project"},
                "base_role": {"type": "string", "description": "Base role to extend: worker or reviewer"},
                "description": {"type": "string", "description": "Short description of what this agent specialises in"},
                "system_prompt_extensions": {"type": "string", "description": "Additional system prompt content appended to the base role prompt"},
                "model_preference": {"type": "string", "description": "Preferred model ID (falls back to project default)"}
            }
        }),
    )
}

pub(crate) fn tool_task_create() -> RmcpTool {
    RmcpTool::new(
        "task_create".to_string(),
        "Create a new task under an epic. Agents should use this only when explicitly allowed by their role and task design.".to_string(),
        object!({
            "type": "object",
            "required": ["epic_id", "title", "acceptance_criteria"],
            "properties": {
                "epic_id": {"type": "string", "description": "Parent epic UUID or short ID"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "design": {"type": "string"},
                "acceptance_criteria": {"type": "array", "items": {}, "description": "Required. Each item is either a plain string or an object with 'criterion' (string) and optional 'met' (bool) fields. Tasks without acceptance criteria cannot be dispatched.", "minItems": 1},
                "issue_type": {"type": "string", "description": "Task type: 'task' (default for worker-routed code work), 'planning' for epic metadata operations (epic_update, epic_close, memory_refs management, roadmap/AC changes, or other metadata-only maintenance), 'spike' for research, 'review' for code review. Use 'planning' when the work requires epic management tools or primarily updates epic metadata instead of code."},
                "priority": {"type": "integer"},
                "owner": {"type": "string"},
                "status": {"type": "string"},
                "parent_id": {"type": "string"},
                "labels": {"type": "array", "items": {"type": "string"}},
                "blocked_by": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) that must complete before this task can be dispatched."}
            }
        }),
    )
}

pub(crate) fn tool_task_update() -> RmcpTool {
    RmcpTool::new(
        "task_update".to_string(),
        "Update task fields and manage blocker relationships. Use blocked_by_add/blocked_by_remove to enforce task sequencing — a task with unresolved blockers will not be dispatched."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "design": {"type": "string"},
                "acceptance_criteria": {"type": "array", "items": {}, "description": "Each item is either a plain string or an object with 'criterion' (string) and optional 'met' (bool) fields."},
                "status": {"type": "string"},
                "priority": {"type": "integer"},
                "owner": {"type": "string"},
                "epic_id": {"type": "string"},
                "memory_refs_add": {"type": "array", "items": {"type": "string"}},
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}},
                "blocked_by_add": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) to add as blockers. Task will not be dispatched until all blockers are resolved."},
                "blocked_by_remove": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) to remove as blockers."}
            }
        }),
    )
}

pub(crate) fn tool_task_transition() -> RmcpTool {
    RmcpTool::new(
        "task_transition".to_string(),
        "Transition a task using a named workflow action.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "action"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "action": {"type": "string", "description": "Transition action name"},
                "reason": {"type": "string", "description": "Reason for the transition. Required for force_close when no replacement_task_ids are provided."},
                "replacement_task_ids": {"type": "array", "items": {"type": "string"}, "description": "UUIDs or short IDs of replacement tasks. Required for force_close when no reason is provided."}
            }
        }),
    )
}

pub(crate) fn tool_task_comment_add() -> RmcpTool {
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
