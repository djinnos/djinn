use crate::extension;
use crate::prompts::TaskContext;
use djinn_core::models::Task;

use super::{AgentRole, RoleConfig};

pub(crate) struct ReviewerRole;

impl AgentRole for ReviewerRole {
    fn config(&self) -> &RoleConfig {
        &REVIEWER_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }
}

pub(crate) const REVIEWER_CONFIG: RoleConfig = RoleConfig {
    name: "reviewer",
    display_name: "Reviewer",
    dispatch_role: "reviewer",
    tool_schemas: extension::tool_schemas_reviewer,
    initial_message: crate::prompts::REVIEWER_TEMPLATE,
    finalize_tool_names: &["submit_review", "request_lead"],
};
