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

pub(crate) static REVIEWER_CONFIG: RoleConfig = RoleConfig {
    name: djinn_roles::config::REVIEWER_CONFIG.name,
    display_name: djinn_roles::config::REVIEWER_CONFIG.display_name,
    dispatch_role: djinn_roles::config::REVIEWER_CONFIG.dispatch_role,
    initial_message: djinn_roles::config::REVIEWER_CONFIG.initial_message,
    finalize_tool_names: djinn_roles::config::REVIEWER_CONFIG.finalize_tool_names,
    mode_section: djinn_roles::config::REVIEWER_CONFIG.mode_section,
};
