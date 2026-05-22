use crate::extension;
use crate::prompts::TaskContext;
use djinn_core::models::Task;

use super::{AgentRole, RoleConfig};

pub(crate) struct ArchitectRole;

impl AgentRole for ArchitectRole {
    fn config(&self) -> &RoleConfig {
        &ARCHITECT_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }
}

pub(crate) const ARCHITECT_CONFIG: RoleConfig = RoleConfig {
    name: "architect",
    display_name: "Architect",
    dispatch_role: "architect",
    tool_schemas: extension::tool_schemas_architect,
    initial_message: crate::prompts::ARCHITECT_TEMPLATE,
    finalize_tool_names: &["submit_work"],
};
