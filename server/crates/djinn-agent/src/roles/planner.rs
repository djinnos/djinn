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

pub(crate) const PLANNER_CONFIG: RoleConfig = RoleConfig {
    name: "planner",
    display_name: "Planner",
    dispatch_role: "planner",
    tool_schemas: extension::tool_schemas_planner,
    initial_message: crate::prompts::PLANNER_TEMPLATE,
    finalize_tool_names: &["submit_grooming"],
};
