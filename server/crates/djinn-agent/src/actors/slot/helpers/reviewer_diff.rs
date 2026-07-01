// ─── b7pe cutover: reviewer-diff helper delegated to djinn-slot ───────────
//
// The canonical `build_reviewer_diff_context` implementation now lives in
// `djinn_slot::helpers::reviewer_diff`.  This module is retained only as an
// agent compatibility shim that provides a thin
// `AgentContext → SlotContext` adapter wrapper so existing agent-side callers
// and tests continue to compile without changes.

use crate::context::AgentContext;
use djinn_core::models::Task;

/// Agent-compatible wrapper around `djinn_slot::helpers::build_reviewer_diff_context`.
///
/// Converts `AgentContext` → `SlotContext` and delegates to the canonical
/// djinn-slot implementation.
pub(crate) async fn build_reviewer_diff_context(
    role_name: &str,
    task: &Task,
    app_state: &AgentContext,
    project_path: &str,
    from_sha: Option<&str>,
    to_sha: Option<&str>,
) -> Option<String> {
    let slot_ctx = super::session_extraction::agent_to_slot_context(app_state);
    djinn_slot::helpers::build_reviewer_diff_context(
        role_name,
        task,
        &slot_ctx,
        project_path,
        from_sha,
        to_sha,
    )
    .await
}
