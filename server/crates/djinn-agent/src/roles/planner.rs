use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::TaskRepository;
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct PlannerRole;

impl AgentRole for PlannerRole {
    fn config(&self) -> &RoleConfig {
        &PLANNER_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }

    fn on_complete<'a>(
        &'a self,
        task_id: &'a str,
        _output: &'a ParsedAgentOutput,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async move {
            // Planning tasks use the simple lifecycle: close on completion.
            let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            if let Ok(Some(task)) = repo.get(task_id).await
                && matches!(task.issue_type.as_str(), "planning" | "decomposition")
            {
                return Some((TransitionAction::Close, None));
            }
            None
        })
    }
}

pub(crate) const PLANNER_CONFIG: RoleConfig = RoleConfig {
    name: "planner",
    display_name: "Planner",
    dispatch_role: "planner",
    tool_schemas: extension::tool_schemas_planner,
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::PLANNER_TEMPLATE,
    preserves_session: false,
    finalize_tool_names: &["submit_grooming"],
};
