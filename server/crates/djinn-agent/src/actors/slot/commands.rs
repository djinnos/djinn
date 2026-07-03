// Re-exports canonical types and wraps `log_commands_run_event` with
// `AgentContext → SlotContext` adapter glue.

use crate::context::AgentContext;
use djinn_core::commands::{CommandResult, CommandSpec};

pub use djinn_slot::{SlotCommand, SlotError};

/// Agent-compatible wrapper around `djinn_slot::commands::log_commands_run_event`.
pub(crate) async fn log_commands_run_event(
    task_id: &str,
    phase: &str,
    specs: &[CommandSpec],
    results: &[CommandResult],
    app_state: &AgentContext,
) {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::commands::log_commands_run_event(task_id, phase, specs, results, slot_ctx)
    });
}
