use super::*;

// ─── Re-exports from canonical djinn-slot feedback helpers ────────────────
//
// Context-free functions are re-exported directly — no adapter overhead.
// Async functions that take `&AgentContext` are wrapped below via the shared
// `with_slot_context!` macro so the adapter construction is not repeated.

pub(crate) use djinn_slot::helpers::{
    COMBINED_BRIEF_SECTION_FLOOR_CHARS, COMBINED_BRIEF_TOTAL_CHARS, budget_combined_sections,
    extract_worker_context, format_command_details, parse_conflict_metadata,
    raw_ci_feedback_in_cycle, recent_feedback, runtime_env_diagnostics, runtime_fs_diagnostics,
};

#[allow(dead_code)] // facade export; currently only called via `initial_user_message_for_task`
pub(crate) async fn pr_review_feedback_context(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<String> {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::helpers::pr_review_feedback_context(task_id, slot_ctx)
    })
}

pub(crate) async fn load_task(task_id: &str, app_state: &AgentContext) -> anyhow::Result<Task> {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::helpers::load_task(task_id, slot_ctx)
    })
}

pub(crate) async fn default_target_branch(project_id: &str, app_state: &AgentContext) -> String {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::helpers::default_target_branch(project_id, slot_ctx)
    })
}

pub(crate) async fn conflict_context_for_dispatch(
    task_id: &str,
    app_state: &AgentContext,
) -> Option<MergeConflictMetadata> {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::helpers::conflict_context_for_dispatch(task_id, slot_ctx)
    })
}

pub(crate) async fn initial_user_message_for_task(
    task_id: &str,
    app_state: &AgentContext,
) -> String {
    crate::with_slot_context!(app_state, |slot_ctx| {
        djinn_slot::helpers::initial_user_message_for_task(task_id, slot_ctx)
    })
}
