use crate::compaction::{GENERIC_PROMPT, SUMMARISER_SYSTEM_GENERIC};
use crate::extension;
use crate::output_parser::ParsedAgentOutput;
use crate::prompts::TaskContext;
use djinn_db::TaskRepository;
use djinn_core::models::{Task, TransitionAction};
use crate::context::AgentContext;
use futures::future::BoxFuture;

use super::{AgentRole, CompactionPrompts, RoleConfig};

pub(crate) struct PmRole;

#[allow(dead_code)]
impl AgentRole for PmRole {
    fn config(&self) -> &RoleConfig {
        &PM_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt(crate::AgentType::PM, task, ctx)
    }

    fn needs_epic_context(&self) -> bool {
        true
    }

    fn on_complete<'a>(
        &'a self,
        task_id: &'a str,
        _output: &'a ParsedAgentOutput,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async move {
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
    tool_schemas: extension::tool_schemas_pm_groomer,
    start_action: |status| match status {
        "needs_pm_intervention" => Some(TransitionAction::PmInterventionStart),
        _ => None,
    },
    release_action: || TransitionAction::PmInterventionRelease,
    initial_message: crate::prompts::PM_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: GENERIC_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_GENERIC,
        pre_resume: GENERIC_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_GENERIC,
    },
    preserves_session: false,
    is_project_scoped: false,
};
