use crate::prompts::TaskContext;
use djinn_core::models::Task;

use super::{AgentRole, RoleConfig};

pub(crate) struct AdvocateRole;

impl AgentRole for AdvocateRole {
    fn config(&self) -> &RoleConfig {
        &ADVOCATE_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }
}

pub(crate) static ADVOCATE_CONFIG: RoleConfig = RoleConfig {
    name: djinn_roles::config::ADVOCATE_CONFIG.name,
    display_name: djinn_roles::config::ADVOCATE_CONFIG.display_name,
    dispatch_role: djinn_roles::config::ADVOCATE_CONFIG.dispatch_role,
    initial_message: djinn_roles::config::ADVOCATE_CONFIG.initial_message,
    finalize_tool_names: djinn_roles::config::ADVOCATE_CONFIG.finalize_tool_names,
    mode_section: djinn_roles::config::ADVOCATE_CONFIG.mode_section,
};
