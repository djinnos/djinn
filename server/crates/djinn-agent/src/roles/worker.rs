use crate::context::AgentContext;
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

/// Worker-specific `RoleConfig` that wires `tool_schemas` to the extension
/// registry via the djinn-roles lookup.
pub(crate) static WORKER_CONFIG: RoleConfig = RoleConfig {
    name: djinn_roles::config::WORKER_CONFIG.name,
    display_name: djinn_roles::config::WORKER_CONFIG.display_name,
    dispatch_role: djinn_roles::config::WORKER_CONFIG.dispatch_role,
    initial_message: djinn_roles::config::WORKER_CONFIG.initial_message,
    finalize_tool_names: djinn_roles::config::WORKER_CONFIG.finalize_tool_names,
    mode_section: djinn_roles::config::WORKER_CONFIG.mode_section,
};
