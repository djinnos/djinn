use crate::context::AgentContext;
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use crate::roles::finalize::SubmitDecision;
use djinn_core::models::{Task, TransitionAction};
use djinn_db::TaskRepository;
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};

pub(crate) struct LeadRole;

impl AgentRole for LeadRole {
    fn config(&self) -> &RoleConfig {
        &LEAD_CONFIG
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
            // ADR-036: use the decision from the finalize payload when present.
            if let Some(payload) = &output.finalize_payload
                && let Ok(decision) = serde_json::from_value::<SubmitDecision>(payload.clone())
            {
                let action = match decision.decision.as_str() {
                    // reopen: complete the intervention and send back to worker.
                    "reopen" => TransitionAction::LeadInterventionComplete,
                    // decompose: the Lead split this task into subtasks — close the
                    // original so it doesn't get re-dispatched to a worker.
                    "decompose" => TransitionAction::ForceClose,
                    // force_close: hard-close the task.
                    "force_close" => TransitionAction::ForceClose,
                    // escalate: release back to the Lead queue (needs_pm_intervention).
                    "escalate" => TransitionAction::LeadInterventionRelease,
                    other => {
                        tracing::warn!(
                            task_id = %task_id,
                            decision = %other,
                            "Lead agent: unrecognized decision value; defaulting to complete"
                        );
                        TransitionAction::LeadInterventionComplete
                    }
                };
                tracing::info!(
                    task_id = %task_id,
                    decision = %decision.decision,
                    "Lead agent: submit_decision → applying transition"
                );
                return Some((action, decision.rationale));
            }

            // Fallback: check if the task already transitioned (pre-ADR-036 Lead tools).
            let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            if let Ok(Some(task)) = repo.get(task_id).await
                && task.status != "in_lead_intervention"
            {
                tracing::info!(
                    task_id = %task_id,
                    current_status = %task.status,
                    "Lead agent: task already transitioned by Lead tools — no fallback needed"
                );
                return None;
            }
            tracing::warn!(
                task_id = %task_id,
                "Lead agent: session ended without explicit completion → releasing back"
            );
            Some((
                TransitionAction::LeadInterventionRelease,
                Some("Lead session ended without completing intervention".to_string()),
            ))
        })
    }
}

pub(crate) const LEAD_CONFIG: RoleConfig = RoleConfig {
    name: "lead",
    display_name: "Lead Intervention",
    dispatch_role: "lead",
    tool_schemas: extension::tool_schemas_lead,
    start_action: |status| match status {
        "needs_lead_intervention" => Some(TransitionAction::LeadInterventionStart),
        _ => None,
    },
    release_action: || TransitionAction::LeadInterventionRelease,
    initial_message: crate::prompts::PM_TEMPLATE,
    preserves_session: false,
    finalize_tool_names: &["submit_decision"],
};
