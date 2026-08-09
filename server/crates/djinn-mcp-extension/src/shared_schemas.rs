// djinn:allow-oversize
// This module centralizes the shared JSON-schema builders consumed by every
// role-specific MCP tool surface; splitting those builders would duplicate
// schema fragments and risk their generated golden snapshots drifting.
use rmcp::model::Tool as RmcpTool;
use rmcp::object;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolSafetyAnnotations {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
    pub concurrent_safe: bool,
}

pub fn tool_proposal_feedback_disposition() -> RmcpTool {
    RmcpTool::new(
        "proposal_feedback_disposition".to_string(),
        "Propose an Advocate disposition for an unresolved `human_feedback` debate entry. Use `fixed_by_revision` with a newer revision on the same proposal, or `wont_fix` with non-empty reasoning. Only the Judge may accept or reject it.".to_string(),
        object!({
            "type": "object", "required": ["id", "disposition"],
            "properties": {
                "id": {"type": "string", "description": "Unresolved human_feedback debate entry id."},
                "disposition": {"type": "string", "enum": ["fixed_by_revision", "wont_fix"]},
                "fixed_by_revision": {"type": "integer", "description": "Required for fixed_by_revision; must be newer and on this proposal."},
                "reason": {"type": "string", "minLength": 1, "description": "Required non-empty reasoning for wont_fix."}
            },
            "allOf": [
                {"if": {"properties": {"disposition": {"const": "fixed_by_revision"}}}, "then": {"required": ["fixed_by_revision"]}},
                {"if": {"properties": {"disposition": {"const": "wont_fix"}}}, "then": {"required": ["reason"]}}
            ]
        }),
    )
}

/// Author withdrawal is exposed to the authenticated chat/control-plane MCP
/// surface, never to a tribunal role surface.
pub fn tool_proposal_feedback_withdraw() -> RmcpTool {
    RmcpTool::new(
        "proposal_feedback_withdraw".to_string(),
        "Withdraw a feedback entry as its original author. This preserves captured source snapshots; a human-feedback obligation closes only when every captured blocking feedback row is withdrawn.".to_string(),
        object!({
            "type": "object", "required": ["id"],
            "properties": {"id": {"type": "string", "description": "Feedback entry id authored by the authenticated caller."}}
        }),
    )
}

pub fn tool_memory_retrieval_outcomes_report() -> RmcpTool {
    RmcpTool::new(
        "memory_retrieval_outcomes_report".to_string(),
        "Read an observational retrieval-injection outcomes report for an explicit project-scoped, timezone-aware RFC-3339 [start,end) interval. This operation is read-only and observational only: it makes no causal or randomized-experiment claim. A task run is deduplicated within each entry_point/rollout_label/outcome cell; cells with different cohort keys can overlap and are non-additive. The response returns the applied interval/timezone plus every database denominator, count, rate, not-applicable state, attempt-number distribution, and unattributed/unrecorded diagnostic. Traces without task_run_id are unattributed and excluded from rates; no fallback through task_id is used. Invalid intervals and requests outside the protected 30-day trace window are rejected without clipping.".to_string(),
        object!({"type":"object", "required":["start","end","timezone"], "properties": {
            "project":{"type":"string"}, "project_id":{"type":"string"},
            "start":{"type":"string", "format":"date-time"}, "end":{"type":"string", "format":"date-time"},
            "timezone":{"type":"string", "minLength":1}
        }}),
    )
}

/// Capture the one-shot investigation plan for an authenticated evidence spike.
pub fn tool_evidence_plan() -> RmcpTool {
    RmcpTool::new(
        "evidence_plan".to_string(),
        "Capture the one-shot evidence investigation plan for this authenticated spike session. Supply a nonempty set of uniquely identified checks using exactly one closed method: code, graph, or command. Task/session identity and provenance are server-derived and cannot be supplied by callers.".to_string(),
        object!({
            "type": "object", "additionalProperties": false, "required": ["checks"],
            "properties": {"checks": {"type": "array", "minItems": 1, "items": {
                "type": "object", "additionalProperties": false,
                "required": ["check_id", "question", "method"],
                "properties": {
                    "check_id": {"type": "string", "minLength": 1, "description": "Nonempty unique check identifier within this plan."},
                    "question": {"type": "string", "minLength": 1, "description": "Nonempty investigation question."},
                    "method": {"type": "string", "enum": ["code", "graph", "command"], "description": "Closed investigation method."}
                }
            }}}
        }),
    )
}

/// Execute a frozen command-method check in the server read-only sandbox.
pub fn tool_evidence_exec() -> RmcpTool {
    RmcpTool::new(
        "evidence_exec".to_string(),
        "Run one frozen command-method evidence check in the server read-only sandbox. Provide a planned check ID and nonempty argv; cwd is an optional relative in-clone directory and timeout_ms is bounded to 1 through 30000. This is argv execution only: command strings, stdin, executable selection, identity, provenance, and sandbox-policy overrides are not accepted.".to_string(),
        object!({
            "type": "object", "additionalProperties": false,
            "required": ["check_id", "argv"],
            "properties": {
                "check_id": {"type": "string", "minLength": 1, "description": "Nonempty ID of a frozen command-method evidence check."},
                "argv": {"type": "array", "minItems": 1, "items": {"type": "string", "minLength": 1}, "description": "Argument vector for the constrained read-only executor; no command-string shell is available."},
                "cwd": {"type": "string", "minLength": 1, "description": "Optional relative directory inside the authenticated clone."},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 30000, "description": "Optional bounded execution timeout in milliseconds (1-30000)."}
            }
        }),
    )
}

/// Inspect persisted memory retrieval traces for the current project.
///
/// The two request forms deliberately live in one tool: list calls stay small
/// enough for operator triage, while detail calls return the selected trace's
/// bounded candidate excerpts.
pub fn tool_memory_recall_trace() -> RmcpTool {
    RmcpTool::new(
        "memory_recall_trace".to_string(),
        "Inspect persisted memory-retrieval traces for the current project. Use mode=list to triage compact trace summaries with optional filters and bounded pagination, or mode=detail with trace_id to inspect one trace and its bounded note excerpts. List/detail expose rollout_label and trace_outcome separately from candidate outcome: rollout_label is the recorded deployment label, trace_outcome is injected, empty, error, legacy_unknown, disabled_off, disabled_kill_switch, or disabled_legacy, while candidate outcomes are injected or skipped. Allowed entry points: dispatch, jit_pitfalls, load_knowledge_context, format_knowledge_notes, memory_recall_trace. Allowed skipped reasons: not_top_k, min_confidence, budget_pruned, oversized_skipped, superseded_pruned, dedupe, search_error.".to_string(),
        object!({
            "type": "object",
            "oneOf": [
                {
                    "required": ["mode"],
                    "properties": {
                        "mode": {"const": "list"},
                        "project": {"type": "string", "description": "Optional project UUID or slug for direct control-plane callers; agent calls are always scoped to the current project."},
                        "project_id": {"type": "string", "description": "Optional project UUID compatibility alias for direct control-plane callers."},
                        "session_id": {"type": "string"},
                        "task_id": {"type": "string"},
                        "task_run_id": {"type": "string"},
                        "entry_point": {"type": "string", "enum": ["dispatch", "jit_pitfalls", "load_knowledge_context", "format_knowledge_notes", "memory_recall_trace"], "description": "Retrieval entry point."},
                        "rollout_label": {"type": "string", "description": "Exact recorded rollout label filter."},
                        "trace_outcome": {"type": "string", "enum": ["injected", "empty", "error", "legacy_unknown", "disabled_off", "disabled_kill_switch", "disabled_legacy"], "description": "Trace-level outcome filter, distinct from candidate outcome."},
                        "outcome": {"type": "string", "enum": ["injected", "skipped"], "description": "Candidate outcome filter."},
                        "skipped_reason": {"type": "string", "enum": ["not_top_k", "min_confidence", "budget_pruned", "oversized_skipped", "superseded_pruned", "dedupe", "search_error"], "description": "Skipped-candidate reason filter."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum compact trace summaries to return (1-100; default is server-defined)."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Zero-based summary page offset."}
                    },
                    "not": {"required": ["trace_id"]}
                },
                {
                    "required": ["mode", "trace_id"],
                    "properties": {
                        "mode": {"const": "detail"},
                        "trace_id": {"type": "string", "minLength": 1, "description": "Trace ID returned from a list call."},
                        "project": {"type": "string", "description": "Optional project UUID or slug for direct control-plane callers; agent calls are always scoped to the current project."},
                        "project_id": {"type": "string", "description": "Optional project UUID compatibility alias for direct control-plane callers."}
                    },
                    "not": {
                        "anyOf": [
                            {"required": ["session_id"]}, {"required": ["task_id"]},
                            {"required": ["task_run_id"]}, {"required": ["entry_point"]},
                            {"required": ["rollout_label"]}, {"required": ["trace_outcome"]},
                            {"required": ["outcome"]}, {"required": ["skipped_reason"]},
                            {"required": ["limit"]}, {"required": ["offset"]}
                        ]
                    }
                }
            ]
        }),
    )
}

impl ToolSafetyAnnotations {
    pub const fn new(
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

    pub const fn read_only() -> Self {
        Self::new(true, false, true, false, true)
    }

    pub const fn open_world_read_only() -> Self {
        Self::new(true, false, true, true, true)
    }

    pub const fn mutation() -> Self {
        Self::new(false, false, false, false, false)
    }

    pub const fn idempotent_mutation() -> Self {
        Self::new(false, false, true, false, false)
    }

    pub const fn destructive() -> Self {
        Self::new(false, true, false, false, false)
    }

    pub const fn idempotent_destructive() -> Self {
        Self::new(false, true, true, false, false)
    }
}

pub fn serialize_tool_schema(
    tool: RmcpTool,
    annotations: ToolSafetyAnnotations,
) -> serde_json::Value {
    let mut value = serde_json::to_value(tool).expect("serialize tool schema");
    annotate_tool_safety(&mut value, annotations);
    value
}

pub fn annotate_tool_safety(value: &mut serde_json::Value, annotations: ToolSafetyAnnotations) {
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

pub fn tool_memory_move() -> RmcpTool {
    RmcpTool::new(
        "memory_move".to_string(),
        "Move a memory note to a different type via memory_* MCP tools. Notes are stored in the project database; use this tool rather than filesystem operations. Updates the permalink and resolves inbound links automatically.".to_string(),
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

pub fn shared_base_tool_schemas() -> Vec<serde_json::Value> {
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

pub fn shared_lead_tool_schemas() -> Vec<serde_json::Value> {
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

pub fn tool_epic_create() -> RmcpTool {
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

pub fn tool_epic_close() -> RmcpTool {
    RmcpTool::new(
        "epic_close".to_string(),
        "Close an epic when all work is complete and no further task waves are needed. Marks the epic as done.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"}
            }
        }),
    )
}

pub fn tool_epic_show() -> RmcpTool {
    RmcpTool::new(
        "epic_show".to_string(),
        "Show full details of an epic by UUID or short ID, including its task breakdown and blocker status.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"}
            }
        }),
    )
}

pub fn tool_epic_update() -> RmcpTool {
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

pub fn tool_epic_tasks() -> RmcpTool {
    RmcpTool::new(
        "epic_tasks".to_string(),
        "List tasks belonging to an epic, showing status and assignee. Supports pagination."
            .to_string(),
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

pub fn tool_epic_blockers_list() -> RmcpTool {
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

pub fn tool_epic_blocked_list() -> RmcpTool {
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

pub fn tool_proposal_show() -> RmcpTool {
    RmcpTool::new(
        "proposal_show".to_string(),
        "Show a proposal's full detail by UUID or short_id. Use `fields` to select sections (proposal, targets, feedback, signoffs, revisions, debate, epics, gate_status; default: all) and `revision_bodies` to control verbosity (excerpt/full/omit).".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Proposal UUID or short_id"},
                "fields": {
                    "type": ["array", "null"],
                    "description": "Select which top-level sections to include. Accepted values: proposal, targets, feedback, signoffs, revisions, debate, epics, gate_status. Default: all. Invalid values return a validation error naming the accepted values.",
                    "items": {"type": "string"},
                    "default": null
                },
                "revision_bodies": {
                    "type": ["string", "null"],
                    "description": "Controls revision body verbosity when revisions is selected: excerpt (default, 512-char body_excerpt), full (complete body), omit (no body data). Ignored when fields omits revisions.",
                    "default": null
                }
            }
        }),
    )
}

pub fn tool_proposal_debate_append() -> RmcpTool {
    RmcpTool::new(
        "proposal_debate_append".to_string(),
        "Record a tribunal debate-trail entry. This is the ONLY channel the refinement loop reads. `kind` is `objection` (Adversary), `verdict` (Judge), or `rebuttal`. `blocking=true` blocks readiness. Read `proposal_id`, `round`, `against_revision_seq` from your task description; `agent_role` is `adversary` or `judge`.".to_string(),
        object!({
            "type": "object",
            "required": ["proposal_id", "kind", "body", "agent_role", "against_revision_seq", "round"],
            "properties": {
                "proposal_id": {"type": "string", "description": "Proposal UUID or short_id (from your task description)"},
                "kind": {"type": "string", "description": "objection | rebuttal | verdict"},
                "body": {"type": "string", "description": "The objection/verdict text. For objections include summary, evidence, and a falsifiable resolution criterion."},
                "blocking": {"type": "boolean", "description": "True if this blocks readiness. Use for blocking objections and for a not-ready verdict. Default false."},
                "agent_role": {"type": "string", "description": "Your tribunal role: adversary | judge"},
                "against_revision_seq": {"type": "integer", "description": "The proposal revision this entry is written against (from your task description)"},
                "round": {"type": "integer", "description": "The 1-based debate round (from your task description)"}
            }
        }),
    )
}

pub fn tool_proposal_debate_list() -> RmcpTool {
    RmcpTool::new(
        "proposal_debate_list".to_string(),
        "List the tribunal debate trail for a proposal: every objection, rebuttal, and verdict across all rounds. The Adversary reads this to avoid re-raising objections; the Advocate reads it to address objections; the Judge reads it to verify resolution.".to_string(),
        object!({
            "type": "object",
            "required": ["proposal_id"],
            "properties": {
                "proposal_id": {"type": "string", "description": "Proposal UUID or short_id"}
            }
        }),
    )
}

pub fn tool_proposal_debate_resolve() -> RmcpTool {
    RmcpTool::new(
        "proposal_debate_resolve".to_string(),
        "Judge-only global resolution for a debate-trail objection by entry `id` (from `proposal_debate_list`). For `human_feedback`, supply `verdict=accept` or `reject`; rejection requires non-empty needs-work `reason`.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "The debate-trail entry id to resolve (from proposal_debate_list)"},
                "verdict": {"type": "string", "enum": ["accept", "reject"], "description": "Required only for human_feedback."},
                "reason": {"type": "string", "minLength": 1, "description": "Required needs-work reasoning when rejecting human_feedback."}
            }
        }),
    )
}

pub fn tool_proposal_refinement_demand_evidence() -> RmcpTool {
    RmcpTool::new(
        "proposal_refinement_demand_evidence".to_string(),
        "Demand a read-only evidence spike for an insufficiently-evidenced feasibility claim. The Judge calls this when in-session research cannot resolve a load-bearing claim. Checks the per-run cap (max 2), records the claim, and parks refinement until spike findings arrive.".to_string(),
        object!({
            "type": "object",
            "required": ["proposal_id", "round", "against_revision_seq", "question", "target_subsystem", "spec_unknown_anchor", "insufficient_in_session_research", "expected_findings", "load_bearing_category"],
            "properties": {
                "proposal_id": {"type": "string", "description": "Proposal UUID or short_id"},
                "round": {"type": "integer", "description": "The 1-based debate round when the demand is issued (from your task description)"},
                "against_revision_seq": {"type": "integer", "description": "The proposal revision sequence the demand targets (from your task description)"},
                "question": {"type": "string", "description": "The feasibility question the evidence spike must answer"},
                "target_subsystem": {"type": "string", "description": "The subsystem or module under investigation"},
                "spec_unknown_anchor": {"type": "string", "description": "What in the spec is unknown or unverified"},
                "insufficient_in_session_research": {"type": "string", "description": "Why in-session research was insufficient to resolve the claim"},
                "expected_findings": {"type": "string", "description": "What the evidence spike should produce to resolve the claim"},
                "load_bearing_category": {"type": "string", "enum": ["feasibility", "safety", "integrity", "compatibility", "rollout", "core_acceptance_criteria"], "description": "Which load-bearing threshold the claim meets. Declare it yourself; the server will not infer it from the question prose."}
            }
        }),
    )
}

pub fn tool_proposal_refinement_retry_evidence() -> RmcpTool {
    RmcpTool::new(
        "proposal_refinement_retry_evidence".to_string(),
        "Retry a failed typed evidence finding using its durable finding_id and exact latest failed_transition_id. Only an authorized Advocate or Judge may retry a finding in failed lifecycle state.".to_string(),
        object!({
            "type": "object", "required": ["finding_id", "failed_transition_id"],
            "properties": {
                "finding_id": {"type": "string", "description": "Typed evidence finding id returned by demand or prior retry."},
                "failed_transition_id": {"type": "string", "description": "Exact latest failed lifecycle transition id to retry."}
            }
        }),
    )
}

pub fn tool_proposal_refinement_resolve_evidence() -> RmcpTool {
    RmcpTool::new(
        "proposal_refinement_resolve_evidence".to_string(),
        "Resolve typed evidence using an applicable validation result and an existing committed folding revision. Only the active Judge may resolve evidence.".to_string(),
        object!({
            "type": "object", "required": ["finding_id", "validation_result_id", "folding_revision", "rationale"],
            "properties": {
                "finding_id": {"type": "string", "description": "Typed evidence finding id to resolve."},
                "validation_result_id": {"type": "string", "description": "Applicable typed validation result id."},
                "folding_revision": {"type": "integer", "description": "Existing committed proposal revision that folds the evidence."},
                "rationale": {"type": "string", "description": "Non-empty Judge rationale for resolution."}
            }
        }),
    )
}

pub fn tool_proposal_refinement_withdraw_evidence() -> RmcpTool {
    RmcpTool::new(
        "proposal_refinement_withdraw_evidence".to_string(),
        "Withdraw typed evidence only with a non-empty rationale and explicit non-load-bearing assertion. Only the active Judge may withdraw evidence.".to_string(),
        object!({
            "type": "object", "required": ["finding_id", "folding_revision", "rationale", "withdrawal_is_non_load_bearing"],
            "properties": {
                "finding_id": {"type": "string", "description": "Typed evidence finding id to withdraw."},
                "folding_revision": {"type": "integer", "description": "Existing committed proposal revision at withdrawal."},
                "rationale": {"type": "string", "description": "Non-empty Judge rationale explaining the withdrawal."},
                "withdrawal_is_non_load_bearing": {"type": "boolean", "description": "Must be true; typed non-load-bearing assertion."}
            }
        }),
    )
}

pub fn tool_proposal_complete() -> RmcpTool {
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

pub fn tool_proposal_ac_set() -> RmcpTool {
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

pub fn tool_proposal_ac_amend() -> RmcpTool {
    RmcpTool::new(
        "proposal_ac_amend".to_string(),
        "Amend a proposal's acceptance-criteria spec with audited revision semantics. Each amendment targets a zero-based criterion index with operation `rewrite`, `drop`, or `waive`; requires a non-empty reason. Bumps proposal revision, retains sign-offs. Use proposal_ac_set for met-flag reconciliation only.".to_string(),
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

pub fn tool_proposal_update() -> RmcpTool {
    RmcpTool::new(
        "proposal_update".to_string(),
        "Update a proposal (by UUID or short_id): title, body, acceptance_criteria, status (draft|shared|ready|archived|superseded), and superseded_by. Only provided fields change.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Proposal UUID or short_id"},
                "title": {"type": "string", "description": "New proposal title"},
                "body": {"type": "string", "description": "New proposal body text"},
                "acceptance_criteria": {
                    "type": "array",
                    "description": "Acceptance criteria: plain strings or {criterion, met} objects",
                    "items": {"type": "object"}
                },
                "status": {"type": "string", "description": "draft | in_review | approved | building | done | rejected | archived | superseded"},
                "superseded_by": {"type": "string", "description": "UUID or short_id of superseding proposal"},
                "body_format": {"type": "string", "description": "Body encoding: markdown (default) or mdx (block-aware)"}
            }
        }),
    )
}

pub fn tool_proposal_block_patch() -> RmcpTool {
    RmcpTool::new(
        "proposal_block_patch".to_string(),
        "Apply a single targeted MDX block patch to a proposal body. Locates a range via selector (heading_text, exact_text, or byte_range), then replaces or wraps it with the given block_mdx. Unrelated content is preserved. Each successful patch records one proposal revision with targeted-block-patch metadata.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "selector", "operation", "block_mdx"],
            "properties": {
                "id": {"type": "string", "description": "Proposal UUID or short_id"},
                "selector": {
                    "type": "object",
                    "description": "Target selector: identifies the range in the body to patch",
                    "properties": {
                        "heading_text": {"type": "string", "description": "Match a markdown heading by text (without # prefix)"},
                        "exact_text": {"type": "string", "description": "Match a contiguous substring of the body (must occur once)"},
                        "byte_range": {
                            "type": "object",
                            "description": "Byte range selector: start inclusive, end exclusive",
                            "properties": {
                                "start": {"type": "integer"},
                                "end": {"type": "integer"},
                                "expected_text": {"type": "string", "description": "Verification text at the range (stale-range guard)"}
                            }
                        }
                    }
                },
                "operation": {"type": "string", "enum": ["replace", "wrap"], "description": "replace replaces the selected range; wrap wraps it"},
                "block_mdx": {"type": "string", "description": "The MDX content to insert"},
                "expected_latest_revision_seq": {"type": "integer", "description": "Stale-revision guard: reject if proposal seq differs"},
                "native_skill_name": {"type": "string", "description": "Name of the native skill producing this patch (e.g. visual-spec)"},
                "native_skill_version": {"type": "string", "description": "Pinned version of the native skill"},
                "note": {"type": "string", "description": "Optional free-form note persisted in revision metadata"}
            }
        }),
    )
}

pub fn tool_get_block_catalog() -> RmcpTool {
    RmcpTool::new(
        "get_block_catalog".to_string(),
        "Return the committed proposal MDX block vocabulary as a lean list of (type, tag) pairs sourced from proposal_block_catalog.json.".to_string(),
        object!({
            "type": "object",
            "properties": {}
        }),
    )
}

pub fn tool_proposal_blocks() -> RmcpTool {
    RmcpTool::new(
        "proposal_blocks".to_string(),
        "Return the v1 proposal MDX block registry, including stable block types, MDX tags, and field schemas.".to_string(),
        object!({
            "type": "object",
            "properties": {}
        }),
    )
}

pub fn tool_proposal_reconcile_obsolete_epic() -> RmcpTool {
    RmcpTool::new(
        "proposal_reconcile_obsolete_epic".to_string(),
        "Scoped teardown for one obsolete graduated epic. Blocks if any target task has merged work; otherwise applies the shared parent-disposition matrix to only the selected linked epic's children: disposed/closed, parked for lead intervention, retained for another open proposal parent, or retained for an external dependent. It then closes and unlinks only the selected epic, leaving unrelated graduated epics linked. Use instead of whole-build proposal_stop_build during Reconcile tasks.".to_string(),
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

pub fn tool_task_list() -> RmcpTool {
    RmcpTool::new(
        "task_list".to_string(),
        "List tasks with optional filters for status, priority, label, and text search. Supports pagination.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "description": "Positive (\"open\") or negative (\"!closed\") status filter. A leading \"!\" matches every task whose status differs from the given value. The pseudo-status \"merged\" matches closed tasks that actually merged (have a merge-commit SHA, or opened a PR and closed as completed) — this is what backs the Kanban Merged column."},
                "issue_type": {"type": "string"},
                "priority": {"type": "integer"},
                "text": {"type": "string", "description": "Free-text search in title/description"},
                "label": {"type": "string"},
                "parent": {"type": "string", "description": "Epic ID to filter by"},
                "sort": {"type": "string", "description": "priority (default), created, created_desc, updated, updated_desc, closed (closed_at DESC then created_at DESC)."},
                "limit": {"type": "integer"},
                "offset": {"type": "integer"}
            }
        }),
    )
}

pub fn tool_task_blocked_list() -> RmcpTool {
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

pub fn tool_task_activity_list() -> RmcpTool {
    RmcpTool::new(
        "task_activity_list".to_string(),
        "Query a task's activity log with optional filters. Returns comments, status transitions, and other events. Use to inspect Lead guidance, reviewer feedback, or history.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "event_type": {"type": "string", "description": "Filter by event type: comment, status_changed, commands_run, merge_conflict, task_review_start"},
                "actor_role": {"type": "string", "description": "Filter by actor: lead, reviewer, worker, system"},
                "limit": {"type": "integer", "description": "Max entries to return (default 30, max 50)"}
            }
        }),
    )
}

pub fn tool_task_show() -> RmcpTool {
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

pub fn tool_memory_read() -> RmcpTool {
    RmcpTool::new(
        "memory_read".to_string(),
        "Read a memory note by permalink or title from the database-backed memory store. Returns full content and metadata.".to_string(),
        object!({
            "type": "object",
            "required": ["identifier"],
            "properties": {
                "identifier": {"type": "string", "description": "Permalink or title"}
            }
        }),
    )
}

pub fn tool_memory_search() -> RmcpTool {
    RmcpTool::new(
        "memory_search".to_string(),
        "Search notes and proposals in project memory. Query formulation rules: (1) write a declarative statement, not an interrogative question; (2) express one information need per query; (3) make each query self-contained; (4) omit retrieval-meta wording such as `find`, `information about`, and `search for`; (5) preserve discriminative symbol names, exact error strings, and config keys verbatim. Good query: `Authentication timeout handling for E_CONNRESET`. Bad query: `Can you find information about authentication timeout errors?` Worker-issued searches remain lexical/BM25-only until proposal 72iu supplies worker embeddings. Returns a unified result set interleaved by relevance.".to_string(),
        object!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {"type": "string"},
                "folder": {"type": "string"},
                "type": {"type": "string"},
                "task_id": {"type": "string", "description": "Task ID for affinity scoring; defaults to the current session task"},
                "limit": {"type": "integer"},
                "edge_kinds": {"type": "array", "items": {"type": "string"}, "description": "Optional list of edge kinds to include in graph traversal scoring. When provided, only edges whose kind matches one of these values participate in spreading activation. Omit to use all edge kinds."},
                "entity_types": {"type": "array", "items": {"type": "string"}, "description": "Optional entity-type filter. Omit to return both notes and proposals (default). [\"note\"] for notes-only. [\"proposal\"] for proposals-only."}
            }
        }),
    )
}

pub fn tool_memory_list() -> RmcpTool {
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

pub fn tool_memory_write() -> RmcpTool {
    RmcpTool::new(
        "memory_write".to_string(),
        "Create a new memory note via the memory_* MCP tools. Notes are stored in the project database; do not use filesystem writes. `type` is required and routes the note (adr, pattern, case, pitfall, research, requirement, reference, design, tech_spike, session, brief, roadmap). Use [[wikilinks]] in content to connect notes.".to_string(),
        object!({
            "type": "object",
            "additionalProperties": false,
            "required": ["reason", "title", "content", "type"],
            "properties": {
                "reason": {"type": "string", "description": "Required audit reason. Unicode whitespace is trimmed and blank values are rejected."},
                "title": {"type": "string", "description": "Note title"},
                "content": {"type": "string", "description": "Markdown content of the note. Use [[wikilinks]] to connect to other notes."},
                "type": {"type": "string", "description": "Note type: adr, pattern, case, pitfall, research, requirement, reference, design, tech_spike, session, brief (singleton), roadmap (singleton)"},
                "status": {"type": "string", "description": "Optional explicit status. For ADRs, use \"proposed\" to mark it as an in-flight proposal."},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Optional tags for categorisation"}
            }
        }),
    )
}

pub fn tool_memory_edit() -> RmcpTool {
    RmcpTool::new(
        "memory_edit".to_string(),
        "Edit an existing memory note in-place via memory_* MCP tools. Notes are stored in the project database; do not use filesystem operations. Operations: append, prepend, find_replace (requires find_text), replace_section (requires section).".to_string(),
        object!({
            "type": "object",
            "additionalProperties": false,
            "required": ["reason", "identifier", "operation", "content"],
            "properties": {
                "reason": {"type": "string", "description": "Required audit reason. Unicode whitespace is trimmed and blank values are rejected."},
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

pub fn tool_memory_build_context() -> RmcpTool {
    RmcpTool::new(
        "memory_build_context".to_string(),
        "Build a curated memory context pack for a task or query by combining note retrieval and ranking. Relevant proposals are surfaced alongside notes so a planner/worker sees the motivating proposal.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Task ID to gather related memory for; defaults to current session task when omitted"},
                "query": {"type": "string", "description": "Optional free-text query to bias retrieval"},
                "limit": {"type": "integer", "description": "Maximum notes to include (default 8)"},
                "min_confidence": {"type": "number", "description": "Minimum confidence threshold for related notes (default 0.1). Notes below this are excluded."},
                "edge_kinds": {"type": "array", "items": {"type": "string"}, "description": "Optional list of edge kinds to include in graph traversal scoring. When provided, only edges whose kind matches one of these values participate in spreading activation. Omit to use all edge kinds."}
            }
        }),
    )
}

pub fn tool_memory_health() -> RmcpTool {
    RmcpTool::new(
        "memory_health".to_string(),
        "Returns aggregate health report: total notes, broken link count, orphan note count, low-confidence note count, stale note count, stale notes by folder, lifecycle counts, and recent lifecycle sweep metrics.".to_string(),
        object!({
            "type": "object",
            "properties": {}
        }),
    )
}

pub fn tool_memory_extracted_audit() -> RmcpTool {
    RmcpTool::new(
        "memory_extracted_audit".to_string(),
        "Audit existing extracted case/pattern/pitfall notes against ADR-054 taxonomy and required structure. Returns grouped cleanup backlogs for merge candidates, underspecified notes, demotion-to-working-spec candidates, and archive candidates, plus rerun guidance.".to_string(),
        object!({
            "type": "object",
            "properties": {}
        }),
    )
}

pub fn tool_memory_broken_links() -> RmcpTool {
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

pub fn tool_memory_orphans() -> RmcpTool {
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

pub fn tool_role_metrics() -> RmcpTool {
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

pub fn tool_role_create() -> RmcpTool {
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

pub fn tool_task_create() -> RmcpTool {
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
                "blocked_by": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) that must complete before this task can be dispatched."},
                "execution_context": {"type": "object", "description": "Optional explicit execution eligibility metadata. Only the complete readiness_guardrail_analysis context is accepted; it is never inferred from task text, labels, roles, or issue type.", "required": ["kind", "skill_name", "skill_version"], "properties": {"kind": {"type": "string", "const": "readiness_guardrail_analysis"}, "skill_name": {"type": "string", "minLength": 1}, "skill_version": {"type": "string", "minLength": 1}}, "additionalProperties": false}
            }
        }),
    )
}

pub fn tool_task_update() -> RmcpTool {
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
                "blocked_by_remove": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) to remove as blockers."},
                "execution_context": {"type": "object", "description": "Complete replacement for explicit execution eligibility metadata. Partial JSON updates are not supported.", "required": ["kind", "skill_name", "skill_version"], "properties": {"kind": {"type": "string", "const": "readiness_guardrail_analysis"}, "skill_name": {"type": "string", "minLength": 1}, "skill_version": {"type": "string", "minLength": 1}}, "additionalProperties": false}
            }
        }),
    )
}

pub fn tool_task_transition() -> RmcpTool {
    RmcpTool::new(
        "task_transition".to_string(),
        "Transition a task to a new status using a named workflow action. Valid transitions depend on the current status.".to_string(),
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

pub fn tool_task_comment_add() -> RmcpTool {
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
