//! Prompt-context assembly: delegates to host callbacks.
use crate::host::SlotContext;
use djinn_core::models::Task;

/// Build the prompt for a task session by delegating to the host.
pub(crate) fn build_prompt_context(task: &Task, role_name: &str, ctx: &SlotContext) -> String {
    let empty_ctx = serde_json::json!({});
    ctx.callbacks.render_prompt(role_name, task, &empty_ctx)
}
