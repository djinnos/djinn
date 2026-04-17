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
        // Per ADR-051 §1/§2 the Architect no longer runs patrols — its dispatch
        // is spike-only (Planner-dispatched or user "Ask architect"). The
        // `next_patrol_minutes` field that used to live on `submit_work` and be
        // logged here has moved to the Planner (see `roles::planner::on_complete`).
        Box::pin(async move { Some((TransitionAction::SubmitForMerge, None)) })
    }
}

pub(crate) const ARCHITECT_CONFIG: RoleConfig = RoleConfig {
    name: "architect",
    display_name: "Architect",
    dispatch_role: "architect",
    tool_schemas: extension::tool_schemas_architect,
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::ARCHITECT_TEMPLATE,
    finalize_tool_names: &["submit_work"],
};
