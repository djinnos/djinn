use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use crate::roles::finalize::SubmitDecision;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::TaskRepository;
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct PmRole;

impl AgentRole for PmRole {
    fn config(&self) -> &RoleConfig {
        &PM_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }

    fn needs_epic_context(&self) -> bool {
        true
    }

    fn on_complete<'a>(
        &'a self,
        task_id: &'a str,
        output: &'a ParsedAgentOutput,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async move {
            // ADR-036: use the decision from the finalize payload when present.
            if let Some(payload) = &output.finalize_payload
                && let Ok(decision) = serde_json::from_value::<SubmitDecision>(payload.clone())
            {
                let action = match decision.decision.as_str() {
                    // reopen: complete the intervention and send back to worker.
                    "reopen" => TransitionAction::PmInterventionComplete,
                    // decompose: the PM split this task into subtasks — close the
                    // original so it doesn't get re-dispatched to a worker.
                    "decompose" => TransitionAction::ForceClose,
                    // force_close: hard-close the task.
                    "force_close" => TransitionAction::ForceClose,
                    // escalate: release back to the PM queue (needs_pm_intervention).
                    "escalate" => TransitionAction::PmInterventionRelease,
                    other => {
                        tracing::warn!(
                            task_id = %task_id,
                            decision = %other,
                            "PM agent: unrecognized decision value; defaulting to complete"
                        );
                        TransitionAction::PmInterventionComplete
                    }
                };
                tracing::info!(
                    task_id = %task_id,
                    decision = %decision.decision,
                    "PM agent: submit_decision → applying transition"
                );
                return Some((action, decision.rationale));
            }

            // Fallback: check if the task already transitioned (pre-ADR-036 PM tools).
            let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            if let Ok(Some(task)) = repo.get(task_id).await
                && task.status != "in_pm_intervention"
            {
                tracing::info!(
                    task_id = %task_id,
                    current_status = %task.status,
                    "PM agent: task already transitioned by PM tools — no fallback needed"
                );
                return None;
            }
            tracing::warn!(
                task_id = %task_id,
                "PM agent: session ended without explicit completion → releasing back"
            );
            Some((
                TransitionAction::PmInterventionRelease,
                Some("PM session ended without completing intervention".to_string()),
            ))
        })
    }
}

pub(crate) const PM_CONFIG: RoleConfig = RoleConfig {
    name: "pm",
    display_name: "PM Intervention",
    dispatch_role: "pm",
    tool_schemas: extension::tool_schemas_pm,
    start_action: |status| match status {
        "needs_pm_intervention" => Some(TransitionAction::PmInterventionStart),
        _ => None,
    },
    release_action: || TransitionAction::PmInterventionRelease,
    initial_message: crate::prompts::PM_TEMPLATE,
    preserves_session: false,
    is_project_scoped: false,
    finalize_tool_names: &["submit_decision"],
};
