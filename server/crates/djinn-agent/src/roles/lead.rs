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

pub(crate) static LEAD_CONFIG: RoleConfig = RoleConfig {
    name: djinn_roles::config::LEAD_CONFIG.name,
    display_name: djinn_roles::config::LEAD_CONFIG.display_name,
    dispatch_role: djinn_roles::config::LEAD_CONFIG.dispatch_role,
    initial_message: djinn_roles::config::LEAD_CONFIG.initial_message,
    finalize_tool_names: djinn_roles::config::LEAD_CONFIG.finalize_tool_names,
    mode_section: djinn_roles::config::LEAD_CONFIG.mode_section,
};
