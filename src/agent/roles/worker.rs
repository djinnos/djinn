use crate::agent::compaction::{
    MID_SESSION_WORKER_PROMPT, PRE_RESUME_WORKER_PROMPT, SUMMARISER_SYSTEM_WORKER_MID_SESSION,
    SUMMARISER_SYSTEM_WORKER_PRE_RESUME,
};
use crate::agent::extension;
use crate::agent::output_parser::ParsedAgentOutput;
use crate::agent::prompts::TaskContext;
use crate::models::{Task, TransitionAction};
use crate::server::AppState;
use futures::future::BoxFuture;

use super::{AgentRole, CompactionPrompts, RoleConfig};
use crate::actors::slot::helpers::initial_user_message_for_task;

pub(crate) struct WorkerRole;

#[allow(dead_code)]
impl AgentRole for WorkerRole {
    fn config(&self) -> &RoleConfig {
        &WORKER_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::agent::prompts::render_prompt(crate::agent::AgentType::Worker, task, ctx)
    }

    fn on_complete<'a>(
        &'a self,
        _task_id: &'a str,
        _output: &'a ParsedAgentOutput,
        _app_state: &'a AppState,
    ) -> BoxFuture<'a, Option<(TransitionAction, Option<String>)>> {
        Box::pin(async { Some((TransitionAction::SubmitVerification, None)) })
    }

    fn initial_user_message<'a>(
        &'a self,
        task_id: &'a str,
        app_state: &'a AppState,
    ) -> BoxFuture<'a, String> {
        Box::pin(initial_user_message_for_task(task_id, app_state))
    }
}

pub(crate) const WORKER_CONFIG: RoleConfig = RoleConfig {
    name: "worker",
    display_name: "Developer",
    dispatch_role: "worker",
    tool_schemas: || extension::tool_schemas(crate::agent::AgentType::Worker),
    start_action: |status| match status {
        "open" => Some(TransitionAction::Start),
        _ => None,
    },
    release_action: || TransitionAction::Release,
    initial_message: crate::agent::prompts::DEV_TEMPLATE,
    compaction: CompactionPrompts {
        mid_session: MID_SESSION_WORKER_PROMPT,
        mid_session_system: SUMMARISER_SYSTEM_WORKER_MID_SESSION,
        pre_resume: PRE_RESUME_WORKER_PROMPT,
        pre_resume_system: SUMMARISER_SYSTEM_WORKER_PRE_RESUME,
    },
    preserves_session: true,
    is_project_scoped: false,
};
