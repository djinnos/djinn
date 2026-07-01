// ─── b7pe cutover: code-context helpers delegated to djinn-slot ───────────
//
// The canonical implementations of `derive_task_scope_paths`,
// `format_knowledge_notes`, `is_role_auto_code_context_enabled`, and
// `build_role_code_graph_context` now live in
// `djinn_slot::helpers::code_context`.  This module is retained only as an
// agent compatibility shim: it re-exports the pure (context-free) helpers and
// provides a thin `AgentContext → SlotContext` adapter wrapper for the
// graph-dependent helper so existing agent-side callers and tests continue to
// compile without changes.

use crate::context::AgentContext;
use djinn_core::models::Task;

// Re-export the pure helpers from the canonical djinn-slot implementation.
pub use djinn_slot::helpers::{
    derive_task_scope_paths, format_knowledge_notes, is_role_auto_code_context_enabled,
};

/// Agent-compatible wrapper around `djinn_slot::helpers::build_role_code_graph_context`.
///
/// Converts `AgentContext` → `SlotContext` and delegates to the canonical
/// djinn-slot implementation.
pub(crate) async fn build_role_code_graph_context(
    role_name: &str,
    task: &Task,
    app_state: &AgentContext,
    project_path: &str,
    task_paths: &[String],
) -> Option<String> {
    let slot_ctx = super::session_extraction::agent_to_slot_context(app_state);
    djinn_slot::helpers::build_role_code_graph_context(
        role_name,
        task,
        &slot_ctx,
        project_path,
        task_paths,
    )
    .await
}
