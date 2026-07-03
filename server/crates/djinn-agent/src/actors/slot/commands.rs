// ─── hfhw cutover: commands delegated to djinn-slot ─────────────────────
// Re-exports canonical types and wraps `log_commands_run_event` with
// `AgentContext → SlotContext` adapter glue.

pub use djinn_slot::{SlotCommand, SlotError};

use crate::context::AgentContext;
use djinn_core::commands::{CommandResult, CommandSpec};

/// Agent-compatible wrapper around `djinn_slot::commands::log_commands_run_event`.
pub(crate) async fn log_commands_run_event(
    task_id: &str,
    phase: &str,
    specs: &[CommandSpec],
    results: &[CommandResult],
    app_state: &AgentContext,
) {
    let slot_ctx = super::session_extraction::agent_to_slot_context(app_state);
    djinn_slot::commands::log_commands_run_event(task_id, phase, specs, results, &slot_ctx).await;
}
