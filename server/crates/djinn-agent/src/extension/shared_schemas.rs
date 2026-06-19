use rmcp::model::Tool as RmcpTool;
use rmcp::object;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolSafetyAnnotations {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
    pub concurrent_safe: bool,
}

impl ToolSafetyAnnotations {
    pub(crate) const fn new(
        read_only: bool,
        destructive: bool,
        idempotent: bool,
        open_world: bool,
        concurrent_safe: bool,
    ) -> Self {
        Self {
            read_only,
            destructive,
            idempotent,
            open_world,
            concurrent_safe,
        }
    }

    pub(crate) const fn read_only() -> Self {
        Self::new(true, false, true, false, true)
    }

    pub(crate) const fn open_world_read_only() -> Self {
        Self::new(true, false, true, true, true)
    }

    pub(crate) const fn mutation() -> Self {
        Self::new(false, false, false, false, false)
    }

    pub(crate) const fn idempotent_mutation() -> Self {
        Self::new(false, false, true, false, false)
    }

    pub(crate) const fn destructive() -> Self {
        Self::new(false, true, false, false, false)
    }

    pub(crate) const fn idempotent_destructive() -> Self {
        Self::new(false, true, true, false, false)
    }
}

pub(crate) fn serialize_tool_schema(
    tool: RmcpTool,
    annotations: ToolSafetyAnnotations,
) -> serde_json::Value {
    let mut value = serde_json::to_value(tool).expect("serialize tool schema");
    annotate_tool_safety(&mut value, annotations);
    value
}

pub(crate) fn annotate_tool_safety(
    value: &mut serde_json::Value,
    annotations: ToolSafetyAnnotations,
) {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "readOnly".to_string(),
            serde_json::Value::Bool(annotations.read_only),
        );
        obj.insert(
            "destructive".to_string(),
            serde_json::Value::Bool(annotations.destructive),
        );
        obj.insert(
            "idempotent".to_string(),
            serde_json::Value::Bool(annotations.idempotent),
        );
        obj.insert(
            "openWorld".to_string(),
            serde_json::Value::Bool(annotations.open_world),
        );
        obj.insert(
            "concurrent_safe".to_string(),
            serde_json::Value::Bool(annotations.concurrent_safe),
        );
    }
}

pub(crate) fn tool_memory_move() -> RmcpTool {
    RmcpTool::new(
        "memory_move".to_string(),
        "Move a memory note to a different type. Memory notes live in Dolt — this is the canonical way to relocate them; do not attempt filesystem rename. Updates the permalink and resolves inbound links automatically. Use type=\"proposed_adr\" to recover a mis-routed ADR draft.".to_string(),
        object!({
            "type": "object",
            "required": ["identifier", "type"],
            "properties": {
                "identifier": {"type": "string", "description": "Note permalink or title"},
                "type": {"type": "string", "description": "New note type. Use proposed_adr to relocate proposal drafts into decisions/proposed/."},
                "title": {"type": "string", "description": "Optional new title; keep current title if omitted."}
            }
        }),
    )
}

pub(crate) fn shared_base_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        serialize_tool_schema(tool_task_show(), ToolSafetyAnnotations::read_only()),
        serialize_tool_schema(tool_task_list(), ToolSafetyAnnotations::read_only()),
        serialize_tool_schema(
            tool_task_activity_list(),
            ToolSafetyAnnotations::read_only(),
        ),
        serialize_tool_schema(tool_memory_read(), ToolSafetyAnnotations::read_only()),
        serialize_tool_schema(tool_memory_search(), ToolSafetyAnnotations::read_only()),
        serialize_tool_schema(tool_memory_list(), ToolSafetyAnnotations::read_only()),
    ]
}

pub(crate) fn shared_lead_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        serialize_tool_schema(tool_task_create(), ToolSafetyAnnotations::mutation()),
        serialize_tool_schema(
            tool_task_update(),
            ToolSafetyAnnotations::idempotent_mutation(),
        ),
        serialize_tool_schema(tool_task_blocked_list(), ToolSafetyAnnotations::read_only()),
        serialize_tool_schema(tool_epic_show(), ToolSafetyAnnotations::read_only()),
        serialize_tool_schema(
            tool_epic_update(),
            ToolSafetyAnnotations::idempotent_mutation(),
        ),
        serialize_tool_schema(tool_epic_tasks(), ToolSafetyAnnotations::read_only()),
        serialize_tool_schema(
            tool_epic_close(),
            ToolSafetyAnnotations::idempotent_mutation(),
        ),
    ]
}

pub(crate) fn tool_epic_create() -> RmcpTool {
    RmcpTool::new(
        "epic_create".to_string(),
        "Create a new epic (top-level grouping entity). Use to open a new strategic thread of work — e.g. when decomposing a graduated proposal (Planner Mode D) or when a health sweep identifies a gap that needs its own delivery container. Returns the created epic.".to_string(),
        object!({
            "type": "object",
            "required": ["title"],
            "properties": {
                "title": {"type": "string", "description": "Epic title"},
                "description": {"type": "string", "description": "Epic description / problem statement"},
                "memory_refs": {"type": "array", "items": {"type": "string"}, "description": "Memory reference URLs (e.g. ADR permalinks) to attach to the epic"},
                "auto_breakdown": {"type": "boolean", "description": "When false, the coordinator will NOT auto-dispatch a breakdown Planner for this epic (stage it without running). Defaults true."},
                "project": {"type": "string", "description": "Target project (UUID or owner/repo slug) to create the epic on. Omit to use the current session's project. Set this to create an epic on a SIBLING repo when decomposing a multi-repo proposal."},
                "read_sources": {"type": "array", "items": {"type": "string"}, "description": "Other registered projects (UUIDs or owner/repo slugs) this epic's tasks may READ while writing only to its own project (cross-repo context)."},
                "proposal_id": {"type": "string", "description": "Proposal (UUID or short_id) this epic is decomposed from — records the proposal→epic link (Mode D)."},
                "blocked_by": {"type": "array", "items": {"type": "string"}, "description": "Epics (UUIDs or short_ids; may be in other repos) that must CLOSE before this epic's breakdown auto-dispatches. Use to sequence epics — e.g. a consumer epic blocked on a schema epic."}
            }
        }),
    )
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
        "Update epic fields (title/description/status), memory ref deltas, and epic dependencies (blocked_by) for planner workflows.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "status": {"type": "string"},
                "memory_refs_add": {"type": "array", "items": {"type": "string"}},
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}},
                "blocked_by_add": {"type": "array", "items": {"type": "string"}, "description": "Epics (UUIDs or short_ids) that must close before this epic's breakdown auto-dispatches."},
                "blocked_by_remove": {"type": "array", "items": {"type": "string"}, "description": "Epic dependencies to remove."}
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

pub(crate) fn tool_epic_blockers_list() -> RmcpTool {
    RmcpTool::new(
        "epic_blockers_list".to_string(),
        "List the epics that BLOCK a given epic (its dependencies — they must close before this epic's breakdown auto-dispatches).".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"}
            }
        }),
    )
}

pub(crate) fn tool_epic_blocked_list() -> RmcpTool {
    RmcpTool::new(
        "epic_blocked_list".to_string(),
        "List the epics blocked BY a given epic (its dependents — epics whose breakdown waits on this one).".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"}
            }
        }),
    )
}

pub(crate) fn tool_proposal_show() -> RmcpTool {
    RmcpTool::new(
        "proposal_show".to_string(),
        "Show a graduated proposal's spec for decomposition (Planner Mode D): title, body, status, acceptance_criteria, and targets (each with project slug + role of `primary`/`reference`). Use this first when dispatched on an `epic_breakdown` task.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Proposal UUID or short ID"}
            }
        }),
    )
}

pub(crate) fn tool_proposal_complete() -> RmcpTool {
    RmcpTool::new(
        "proposal_complete".to_string(),
        "Mark a `building` proposal as `done` (Planner Workflow E). Call this only after reviewing a proposal whose every graduated epic has closed and confirming the delivered work meets the acceptance criteria. If work remains instead, create more epics with epic_create(proposal_id=...) rather than completing.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Proposal UUID or short ID"},
                "summary": {"type": "string", "description": "Short note on what shipped and how it satisfies the spec (recorded in logs)."}
            }
        }),
    )
}

pub(crate) fn tool_proposal_ac_set() -> RmcpTool {
    RmcpTool::new(
        "proposal_ac_set".to_string(),
        "Reconcile a proposal's acceptance-criteria `met` flags (Planner Workflow E) as graduated epics land. Send the FULL list in order — one entry per criterion, each `{\"met\": true|false}` (criterion text is preserved automatically). A status annotation only: it does not edit the spec, bump a revision, or clear sign-offs. Returns {met, total}.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "acceptance_criteria"],
            "properties": {
                "id": {"type": "string", "description": "Proposal UUID or short ID"},
                "acceptance_criteria": {
                    "type": "array",
                    "description": "Full criteria list in the same order as proposal_show; each entry {\"met\": bool} (optionally with \"criterion\").",
                    "items": {
                        "type": "object",
                        "properties": {
                            "criterion": {"type": "string"},
                            "met": {"type": "boolean"}
                        }
                    }
                }
            }
        }),
    )
}

pub(crate) fn tool_proposal_ac_amend() -> RmcpTool {
    RmcpTool::new(
        "proposal_ac_amend".to_string(),
        "Amend a proposal's acceptance-criteria spec with audited revision semantics. Each amendment targets a zero-based criterion index and uses operation `rewrite`, `drop`, or `waive`: rewrite replaces criterion text and requires `criterion`; drop removes the criterion; waive keeps the criterion visible with `waived: true`. Requires a non-empty top-level reason. This is a real spec edit: it bumps the proposal revision, retains sign-offs, and records feedback/audit. Use proposal_ac_set instead when only reconciling met flags.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason", "amendments"],
            "properties": {
                "id": {"type": "string", "description": "Proposal UUID or short ID"},
                "reason": {"type": "string", "minLength": 1, "description": "Required non-empty explanation for the acceptance-criteria amendment audit trail"},
                "amendments": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more spec amendments applied in order. Operations target zero-based acceptance-criteria indexes from proposal_show; drops affect later indexes, so order multi-drop operations carefully.",
                    "items": {
                        "type": "object",
                        "required": ["operation", "index"],
                        "properties": {
                            "operation": {"type": "string", "enum": ["rewrite", "drop", "waive"], "description": "Amendment operation: rewrite replaces criterion text; drop removes the criterion; waive keeps it but marks it with waived: true."},
                            "index": {"type": "integer", "minimum": 0, "description": "Zero-based acceptance-criteria index to amend."},
                            "criterion": {"type": "string", "minLength": 1, "description": "New criterion text; required and non-empty when operation is rewrite."}
                        },
                        "allOf": [
                            {
                                "if": {"properties": {"operation": {"const": "rewrite"}}},
                                "then": {"required": ["criterion"]}
                            }
                        ]
                    }
                }
            }
        }),
    )
}

pub(crate) fn tool_proposal_reconcile_obsolete_epic() -> RmcpTool {
    RmcpTool::new(
        "proposal_reconcile_obsolete_epic".to_string(),
        "Scoped proposal-reconcile teardown for one obsolete graduated epic. Lists/validates only the requested proposal+epic link, blocks terminally if any task in that subtree has merged work (recording AI proposal feedback), otherwise force-closes only that epic's tasks, closes the epic, unlinks only that epic from the proposal, and leaves unrelated graduated epics untouched. Use this instead of whole-build proposal_stop_build during Reconcile proposal tasks.".to_string(),
        object!({
            "type": "object",
            "required": ["proposal_id", "epic_id"],
            "properties": {
                "proposal_id": {"type": "string", "description": "Proposal UUID or short ID being reconciled"},
                "epic_id": {"type": "string", "description": "Obsolete graduated epic UUID or short ID to retire"},
                "reason": {"type": "string", "description": "Optional reconcile note explaining why this graduated epic is obsolete"}
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
                "status": {"type": "string", "description": "Explicit lifecycle status filter. Defaults to active; use archived or deprecated to list non-live notes."},
                "depth": {"type": "integer", "description": "Depth control: 0 = unlimited, 1 = exact folder (default), N = N levels"}
            }
        }),
    )
}

pub(crate) fn tool_memory_write() -> RmcpTool {
    RmcpTool::new(
        "memory_write".to_string(),
        "Create a new memory note. Memory notes live in Dolt — this is the canonical way to author them; do not attempt filesystem writes. `type` is required and routes the note (adr, pattern, case, pitfall, research, requirement, reference, design, tech_spike, session, brief [singleton], roadmap [singleton]). Use [[wikilinks]] in content to connect notes.".to_string(),
        object!({
            "type": "object",
            "required": ["title", "content", "type"],
            "properties": {
                "title": {"type": "string", "description": "Note title"},
                "content": {"type": "string", "description": "Markdown content of the note. Use [[wikilinks]] to connect to other notes."},
                "type": {"type": "string", "description": "Note type: adr, pattern, case, pitfall, research, requirement, reference, design, tech_spike, session, brief (singleton), roadmap (singleton)"},
                "status": {"type": "string", "description": "Optional explicit status. For ADRs, use \"proposed\" to mark it as an in-flight proposal."},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional tags for categorisation"}
            }
        }),
    )
}

pub(crate) fn tool_memory_edit() -> RmcpTool {
    RmcpTool::new(
        "memory_edit".to_string(),
        "Edit an existing memory note in-place. Memory notes live in Dolt — this is the canonical way to amend them; do not attempt filesystem writes. Operations: \"append\" (add to end), \"prepend\" (add after frontmatter), \"find_replace\" (exact text replacement, requires find_text), \"replace_section\" (replace content under a markdown heading, requires section).".to_string(),
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
        "Returns aggregate health report: total notes, broken link count, orphan note count, low-confidence note count, stale note count, stale notes by folder, lifecycle counts, and recent lifecycle sweep metrics.".to_string(),
        object!({
            "type": "object",
            "properties": {}
        }),
    )
}

pub(crate) fn tool_memory_extracted_audit() -> RmcpTool {
    RmcpTool::new(
        "memory_extracted_audit".to_string(),
        "Audit existing extracted case/pattern/pitfall notes against ADR-054 taxonomy and required structure. Returns grouped cleanup backlogs for merge candidates, underspecified notes, demotion-to-working-spec candidates, and archive candidates, plus rerun guidance.".to_string(),
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
