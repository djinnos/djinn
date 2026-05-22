use crate::extension;
use crate::prompts::TaskContext;
use djinn_core::models::Task;

use super::{AgentRole, RoleConfig};

pub(crate) struct LeadRole;

impl AgentRole for LeadRole {
    fn config(&self) -> &RoleConfig {
        &LEAD_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }
}

pub(crate) const LEAD_CONFIG: RoleConfig = RoleConfig {
    name: "lead",
    display_name: "Lead Intervention",
    dispatch_role: "lead",
    tool_schemas: extension::tool_schemas_lead,
    initial_message: crate::prompts::LEAD_TEMPLATE,
    finalize_tool_names: &["submit_decision"],
};
