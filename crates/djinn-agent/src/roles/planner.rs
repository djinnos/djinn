use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct PlannerRole;

impl AgentRole for PlannerRole {
    fn config(&self) -> &RoleConfig {
        &PLANNER_CONFIG
    }

    fn render_prompt(&self, _task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_project_prompt_for_role(
            self.config(),
            &ctx.project_path,
            ctx.verification_commands.as_deref(),
        )
    }

    fn on_complete<'a>(
        &'a self,
        _task_id: &'a str,
        _output: &'a ParsedAgentOutput,
        _app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async { None })
    }
}

pub(crate) const PLANNER_CONFIG: RoleConfig = RoleConfig {
    name: "planner",
    display_name: "Planner",
    dispatch_role: "planner",
    tool_schemas: extension::tool_schemas_planner,
    start_action: |_status| None,
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::PLANNER_TEMPLATE,
    preserves_session: false,
    is_project_scoped: true,
    finalize_tool_names: &["submit_grooming"],
};
