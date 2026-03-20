use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct ArchitectRole;

impl AgentRole for ArchitectRole {
    fn config(&self) -> &RoleConfig {
        &ARCHITECT_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }

    fn on_complete<'a>(
        &'a self,
        _task_id: &'a str,
        _output: &'a ParsedAgentOutput,
        _app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        // Architect tasks use the simple lifecycle: on complete, close the task.
        Box::pin(async { Some((TransitionAction::Close, None)) })
    }
}

pub(crate) const ARCHITECT_CONFIG: RoleConfig = RoleConfig {
    name: "architect",
    display_name: "Architect",
    dispatch_role: "architect",
    tool_schemas: extension::tool_schemas_worker,
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::DEV_TEMPLATE,
    preserves_session: false,
    is_project_scoped: false,
    finalize_tool_names: &["submit_work"],
};
