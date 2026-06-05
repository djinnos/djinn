use crate::extension;
use crate::prompts::TaskContext;
use djinn_core::models::Task;

use super::{AgentRole, RoleConfig};

pub(crate) struct PlannerRole;

impl AgentRole for PlannerRole {
    fn config(&self) -> &RoleConfig {
        &PLANNER_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }
}

// Mode workflow sections — exactly one is injected at `{{role_mode_section}}`
// based on the dispatched task. The LLM no longer "detects" its mode.
const PLANNER_PATROL: &str = include_str!("../prompts/planner/patrol.md");
const PLANNER_DECOMPOSITION: &str = include_str!("../prompts/planner/decomposition.md");
const PLANNER_INTERVENTION: &str = include_str!("../prompts/planner/intervention.md");
const PLANNER_PROPOSAL: &str = include_str!("../prompts/planner/proposal.md");
const PLANNER_PROPOSAL_REVIEW: &str = include_str!("../prompts/planner/proposal_review.md");

/// Select the Planner workflow section for this dispatch. Mirrors the planner
/// arms of [`crate::roles::flow_for_task_dispatch`] — keep them in sync.
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
            PLANNER_PROPOSAL_REVIEW
        }
        "epic_breakdown" => PLANNER_PROPOSAL,
        "planning" | "decomposition" => PLANNER_DECOMPOSITION,
        // Review tasks split by whether they are the periodic board patrol or a
        // targeted escalation/stuck-task intervention.
        "review" if task.title.to_lowercase().contains("patrol") => PLANNER_PATROL,
        "review" => PLANNER_INTERVENTION,
        // Planner only ever runs these issue types; default to decomposition as
        // the safe catch-all rather than leaving the section empty.
        _ => PLANNER_DECOMPOSITION,
    }
}

pub(crate) const PLANNER_CONFIG: RoleConfig = RoleConfig {
    name: "planner",
    display_name: "Planner",
    dispatch_role: "planner",
    tool_schemas: extension::tool_schemas_planner,
    initial_message: crate::prompts::PLANNER_TEMPLATE,
    finalize_tool_names: &["submit_grooming"],
    mode_section: Some(planner_mode_section),
};
