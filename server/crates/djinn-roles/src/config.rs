//! Role configuration data.
//!
//! Each role has a static [`RoleConfig`] that captures its display name,
//! dispatch role, prompt template, mode section selector, and finalize tool
//! names.  The `tool_schemas` function pointer is **not** stored here;
//! tool schema providers are registered at startup via
//! [`crate::register_tool_schemas`] to avoid a compile-time dependency on
//! `djinn-agent::extension`.

use crate::AgentType;
use crate::prompts;
use djinn_core::models::Task;

/// Runtime context injected alongside the task's stored fields at render time.
///
/// Re-exported from [`crate::prompts`] for convenience.
pub use crate::prompts::TaskContext;

/// Static configuration for an agent role.
///
/// These are compile-time constants — one per role variant.  Tool schema
/// providers are resolved at runtime through the [`crate::register_tool_schemas`]
/// registry rather than embedded as function pointers, keeping the dependency
/// graph acyclic.
#[derive(Clone, Copy)]
pub struct RoleConfig {
    pub name: &'static str,
    pub display_name: &'static str,
    pub dispatch_role: &'static str,
    pub initial_message: &'static str,
    /// Tool names the agent can call to signal completion for this role.
    /// The first entry is the primary finalize tool; additional entries are
    /// alternate exit paths (e.g. `request_lead` for workers).
    pub finalize_tool_names: &'static [&'static str],
    /// Mode selector: given the task + render context, return the single
    /// mode-specific prompt section to inject at `{{role_mode_section}}`.
    /// `None` for single-mode roles (their template has no placeholder).
    ///
    /// This is how the *dispatcher* (code, not the LLM) picks the workflow:
    /// instead of the prompt asking the model to "detect your mode", the
    /// builder injects only the relevant mode's instructions. Keep each
    /// selector in lockstep with [`flow_for_task_dispatch`] /
    /// [`role_for_task_dispatch`].
    pub mode_section: Option<fn(&Task, &TaskContext) -> &'static str>,
}

// ─── Per-role static configs ─────────────────────────────────────────────────

/// Worker role: open/in_progress tasks, research, conflict resolution.
fn worker_mode_section(task: &Task, ctx: &TaskContext) -> &'static str {
    let in_conflict = ctx
        .conflict_files
        .as_deref()
        .is_some_and(|f| !f.trim().is_empty())
        || ctx
            .merge_failure_context
            .as_deref()
            .is_some_and(|f| !f.trim().is_empty());
    if in_conflict {
        return prompts::WORKER_CONFLICT;
    }
    match task.issue_type.as_str() {
        "research" => prompts::WORKER_RESEARCH,
        _ => "",
    }
}

pub const WORKER_CONFIG: RoleConfig = RoleConfig {
    name: "worker",
    display_name: "Developer",
    dispatch_role: "worker",
    initial_message: prompts::DEV_TEMPLATE,
    finalize_tool_names: &["submit_work", "request_lead"],
    mode_section: Some(worker_mode_section),
};

pub const REVIEWER_CONFIG: RoleConfig = RoleConfig {
    name: "reviewer",
    display_name: "Reviewer",
    dispatch_role: "reviewer",
    initial_message: prompts::REVIEWER_TEMPLATE,
    finalize_tool_names: &["submit_review", "request_lead"],
    mode_section: None,
};

pub const LEAD_CONFIG: RoleConfig = RoleConfig {
    name: "lead",
    display_name: "Lead Intervention",
    dispatch_role: "lead",
    initial_message: prompts::LEAD_TEMPLATE,
    finalize_tool_names: &["submit_decision"],
    mode_section: None,
};

/// Planner mode section selector.
///
/// Mirrors the planner arms of `flow_for_task_dispatch` — keep them in sync.
fn planner_mode_section(task: &Task, _ctx: &TaskContext) -> &'static str {
    match task.issue_type.as_str() {
        // `epic_breakdown` splits by title: the coordinator stamps the
        // review-marker prefix when all graduated epics of a proposal close
        // (Workflow E), otherwise it's the initial decomposition (Workflow D).
        "epic_breakdown"
            if task
                .title
                .starts_with(djinn_core::models::task::PROPOSAL_REVIEW_TITLE_PREFIX) =>
        {
            prompts::PLANNER_PROPOSAL_REVIEW
        }
        "epic_breakdown" => prompts::PLANNER_PROPOSAL,
        "planning" | "decomposition" => prompts::PLANNER_DECOMPOSITION,
        // Review tasks are targeted escalation/stuck-task interventions.
        "review" => prompts::PLANNER_INTERVENTION,
        // Planner only ever runs these issue types; default to decomposition as
        // the safe catch-all rather than leaving the section empty.
        _ => prompts::PLANNER_DECOMPOSITION,
    }
}

pub const PLANNER_CONFIG: RoleConfig = RoleConfig {
    name: "planner",
    display_name: "Planner",
    dispatch_role: "planner",
    initial_message: prompts::PLANNER_TEMPLATE,
    finalize_tool_names: &["submit_grooming"],
    mode_section: Some(planner_mode_section),
};

pub const ARCHITECT_CONFIG: RoleConfig = RoleConfig {
    name: "architect",
    display_name: "Architect",
    dispatch_role: "architect",
    initial_message: prompts::ARCHITECT_TEMPLATE,
    finalize_tool_names: &["submit_work"],
    mode_section: None,
};

/// Advocate role: proposal author/reviser in the tribunal refinement workflow (k9zw).
pub const ADVOCATE_CONFIG: RoleConfig = RoleConfig {
    name: "advocate",
    display_name: "Proposal Advocate",
    dispatch_role: "advocate",
    initial_message: prompts::ADVOCATE_TEMPLATE,
    finalize_tool_names: &["submit_work"],
    mode_section: None,
};

/// Adversary role: red-team objection producer in the tribunal refinement workflow (k9zw).
pub const ADVERSARY_CONFIG: RoleConfig = RoleConfig {
    name: "adversary",
    display_name: "Proposal Adversary",
    dispatch_role: "adversary",
    initial_message: prompts::ADVERSARY_TEMPLATE,
    finalize_tool_names: &["submit_review"],
    mode_section: None,
};

/// Judge role: independent adjudicator in the tribunal refinement workflow (k9zw).
pub const JUDGE_CONFIG: RoleConfig = RoleConfig {
    name: "judge",
    display_name: "Proposal Judge",
    dispatch_role: "judge",
    initial_message: prompts::JUDGE_TEMPLATE,
    finalize_tool_names: &["submit_decision"],
    mode_section: None,
};

/// Resolve the base `RoleConfig` for an `AgentType`.
pub fn config_for(agent_type: AgentType) -> &'static RoleConfig {
    match agent_type {
        AgentType::Worker => &WORKER_CONFIG,
        AgentType::Reviewer => &REVIEWER_CONFIG,
        AgentType::Lead => &LEAD_CONFIG,
        AgentType::Planner => &PLANNER_CONFIG,
        AgentType::Architect => &ARCHITECT_CONFIG,
        AgentType::Advocate => &ADVOCATE_CONFIG,
        AgentType::Adversary => &ADVERSARY_CONFIG,
        AgentType::Judge => &JUDGE_CONFIG,
    }
}

/// Returns `true` if the task's `issue_type` is `planning` (simple lifecycle,
/// dispatched to the Planner role). Also matches legacy `decomposition` for
/// backward compatibility with existing DB rows.
pub fn planning_claims(task: &Task) -> bool {
    matches!(
        task.issue_type.as_str(),
        "planning" | "decomposition" | "epic_breakdown"
    )
}
