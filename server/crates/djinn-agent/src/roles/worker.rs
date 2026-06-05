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

// Mode workflow sections injected at `{{role_mode_section}}`. The default
// "implement" flow needs no extra section — all of its guidance lives in the
// base template — so it injects nothing.
const WORKER_RESEARCH: &str = include_str!("../prompts/worker/research.md");
const WORKER_CONFLICT: &str = include_str!("../prompts/worker/conflict.md");

/// Select the Worker workflow section for this dispatch. Conflict recovery
/// wins (the merge must be resolved before anything else); then research tasks
/// (note-deliverable, not code); otherwise the normal implement flow, which
/// adds no mode section.
fn worker_mode_section(task: &Task, ctx: &TaskContext) -> &'static str {
    let in_conflict = ctx
        .conflict_files
        .as_deref()
        .is_some_and(|f| !f.trim().is_empty())
        || ctx
            .merge_failure_context
            .as_deref()
            .is_some_and(|f| !f.trim().is_empty());
    if in_conflict {
        return WORKER_CONFLICT;
    }
    match task.issue_type.as_str() {
        "research" => WORKER_RESEARCH,
        _ => "",
    }
}

pub(crate) const WORKER_CONFIG: RoleConfig = RoleConfig {
    name: "worker",
    display_name: "Developer",
    dispatch_role: "worker",
    tool_schemas: extension::tool_schemas_worker,
    initial_message: crate::prompts::DEV_TEMPLATE,
    finalize_tool_names: &["submit_work", "request_lead"],
    mode_section: Some(worker_mode_section),
};
