// ─── hfhw cutover: commands delegated to djinn-slot ─────────────────────
//
// The canonical `SlotCommand` enum, `SlotError` type, and
// `log_commands_run_event` helper now live in `djinn_slot::commands`.
//
// This module re-exports those canonical types and provides a thin
// `AgentContext → SlotContext` adapter wrapper for
// `log_commands_run_event` so existing agent-side callers continue to
// compile without changes.

// Re-export canonical types from djinn-slot so `super::commands::SlotCommand`,
// `super::commands::SlotError`, etc. continue to resolve for agent-internal
// callers (actor.rs, pool/actor.rs, supervisor_runner.rs, etc.).
pub use djinn_slot::{SlotCommand, SlotError};

use crate::context::AgentContext;
use djinn_core::commands::{CommandResult, CommandSpec};

/// Agent-compatible wrapper around `djinn_slot::commands::log_commands_run_event`.
///
/// Converts `AgentContext` → `SlotContext` and delegates to the canonical
/// djinn-slot implementation.
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
