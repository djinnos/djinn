//! Lifecycle teardown: delegates to host callbacks.
use crate::host::SlotContext;
use crate::output_parser::ParsedAgentOutput;
use crate::roles_support::AgentRole;
use std::sync::Arc;

pub(crate) struct PostSessionParams {
    pub(crate) task_id: String,
    pub(crate) project_path: String,
    pub(crate) role: Arc<dyn AgentRole>,
    pub(crate) ctx: SlotContext,
    pub(crate) final_output: ParsedAgentOutput,
    pub(crate) final_result_ok: bool,
    pub(crate) final_error: Option<String>,
    pub(crate) tokens_in: i64,
    pub(crate) tokens_out: i64,
}

pub(crate) fn spawn_post_session_work(params: PostSessionParams) {
    params.ctx.register_background_work(&params.task_id);
    let ctx = params.ctx.clone();
    let task_id = params.task_id.clone();
    tokio::spawn(async move {
        // Host-side finalize + transition logic is delegated through callbacks.
        ctx.deregister_background_work(&task_id);
    });
}

pub(crate) async fn apply_transition_and_dispatch(
    _transition: Option<(djinn_core::models::TransitionAction, Option<String>)>,
    task_id: &str,
    _project_path: &str,
    _role: &Arc<dyn AgentRole>,
    ctx: &SlotContext,
    _tokens_in: i64,
    _tokens_out: i64,
) {
    if let Ok(task) = ctx.load_task(task_id).await {
        ctx.trigger_dispatch_for_project(&task.project_id).await;
    }
}
