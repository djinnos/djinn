use crate::context::AgentContext;
use crate::extension;
use crate::prompts::TaskContext;
use djinn_core::models::Task;
use futures::future::BoxFuture;

use super::{AgentRole, RoleConfig};
use crate::actors::slot::helpers::initial_user_message_for_task;

pub(crate) struct WorkerRole;

impl AgentRole for WorkerRole {
    fn config(&self) -> &RoleConfig {
        &WORKER_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }

    fn initial_user_message<'a>(
        &'a self,
        task_id: &'a str,
        app_state: &'a AgentContext,
    ) -> BoxFuture<'a, String> {
        Box::pin(initial_user_message_for_task(task_id, app_state))
    }
}

pub(crate) const WORKER_CONFIG: RoleConfig = RoleConfig {
    name: "worker",
    display_name: "Developer",
    dispatch_role: "worker",
    tool_schemas: extension::tool_schemas_worker,
    initial_message: crate::prompts::DEV_TEMPLATE,
    finalize_tool_names: &["submit_work", "request_lead"],
};
