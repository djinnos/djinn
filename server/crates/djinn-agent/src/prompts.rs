// Embedded prompt templates for Djinn agent types.
//
// After Phase 3, prompt templates, `TaskContext`, and core rendering live in
// [`djinn_roles::prompts`].  This module re-exports those public symbols under
// the old `djinn_agent::prompts::*` paths so existing consumers keep compiling
// unchanged, and adds agent-specific prompt helpers that depend on
// `djinn-agent` internals.

// ─── Re-exports from djinn-roles (facade paths) ─────────────────────────────

pub use djinn_roles::prompts::MAX_SYSTEM_PROMPT_CHARS;
pub use djinn_roles::prompts::TaskContext;
pub use djinn_roles::prompts::format_tool_line_with_description;
pub use djinn_roles::prompts::format_tools_section;
pub use djinn_roles::prompts::{
    ADVERSARY_TEMPLATE, ADVOCATE_TEMPLATE, ARCHITECT_TEMPLATE, BASE_TEMPLATE, CLUSTER_DOC_TEMPLATE,
    DEV_TEMPLATE, JUDGE_TEMPLATE, LEAD_TEMPLATE, PLANNER_TEMPLATE, REVIEWER_TEMPLATE,
};
pub use djinn_roles::prompts::{format_acceptance_criteria, format_labels};

/// Role-based variant of `render_prompt` — resolves tool schemas from the
/// djinn-roles registry.
pub fn render_prompt_for_role(
    config: &djinn_roles::config::RoleConfig,
    task: &djinn_core::models::Task,
    ctx: &TaskContext,
) -> String {
    let tool_schemas_fn = djinn_roles::tool_schemas_fn_for(
        crate::AgentType::parse(config.name).unwrap_or(crate::AgentType::Worker),
    )
    .unwrap_or(Vec::new);

    djinn_roles::prompts::render_prompt_for_role(config, tool_schemas_fn, task, ctx)
}

// ─── Agent-specific prompt helpers ───────────────────────────────────────────

/// Render a system prompt for `agent_type` using data from `task` and `ctx`.
///
/// Test-only convenience wrapper — production code uses `render_prompt_for_role`.
#[cfg(test)]
pub fn render_prompt(
    agent_type: crate::AgentType,
    task: &djinn_core::models::Task,
    ctx: &TaskContext,
) -> String {
    let config = agent_type.role_config();
    render_prompt_for_role(config, task, ctx)
}

/// Append per-role prompt extensions to a fully-rendered system prompt.
///
/// Order: base rendered prompt → `system_prompt_extensions`.
/// Empty or whitespace-only values are skipped.
/// Called by the execution layer after resolving the applicable DB agent_role.
pub fn apply_role_extensions(base: &str, system_prompt_extensions: &str) -> String {
    let mut out = base.to_string();
    if !system_prompt_extensions.trim().is_empty() {
        out.push_str("\n\n");
        out.push_str(system_prompt_extensions.trim());
    }
    out
}

/// Append the skills section to a system prompt.
///
/// Called after `apply_role_extensions`. Appends the formatted "## Available Skills"
/// section when any skills were resolved. Returns the prompt unchanged when empty.
pub fn apply_skills(prompt: &str, skills: &[crate::skills::ResolvedSkill]) -> String {
    let section = crate::skills::format_skills_section(skills);
    if section.is_empty() {
        return prompt.to_string();
    }
    let mut out = prompt.to_string();
    out.push_str("\n\n");
    out.push_str(&section);
    out
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
