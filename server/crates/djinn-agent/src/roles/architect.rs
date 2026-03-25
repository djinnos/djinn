use crate::actors::coordinator::rules;
use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::TaskRepository;
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
        task_id: &'a str,
        output: &'a ParsedAgentOutput,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async move {
            // Extract next_patrol_minutes from the finalize payload if present.
            if let Some(minutes) = output
                .finalize_payload
                .as_ref()
                .and_then(|p| p.get("next_patrol_minutes"))
                .and_then(|v| v.as_u64())
            {
                let minutes = (minutes as u32).clamp(
                    rules::MIN_ARCHITECT_PATROL_MINUTES,
                    rules::MAX_ARCHITECT_PATROL_MINUTES,
                );
                let task_repo =
                    TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                let payload_json =
                    serde_json::json!({ "next_patrol_minutes": minutes }).to_string();
                if let Err(e) = task_repo
                    .log_activity(
                        Some(task_id),
                        "architect",
                        "architect",
                        "patrol_schedule",
                        &payload_json,
                    )
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        task_id,
                        "Architect: failed to log patrol_schedule activity"
                    );
                } else {
                    tracing::info!(
                        task_id,
                        next_patrol_minutes = minutes,
                        "Architect: patrol self-scheduled next run"
                    );
                }
            }

            Some((TransitionAction::Close, None))
        })
    }
}

pub(crate) const ARCHITECT_CONFIG: RoleConfig = RoleConfig {
    name: "architect",
    display_name: "Architect",
    dispatch_role: "architect",
    tool_schemas: extension::tool_schemas_architect,
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::prompts::ARCHITECT_TEMPLATE,
    preserves_session: false,
    finalize_tool_names: &["submit_work"],
};
