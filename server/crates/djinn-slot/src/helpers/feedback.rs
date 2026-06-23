//! Feedback helpers (stub).
use crate::host::SlotContext;
use crate::truncate::smart_truncate;
use djinn_core::models::Task;

pub(crate) const COMBINED_BRIEF_SECTION_FLOOR_CHARS: usize = 500;
pub(crate) const COMBINED_BRIEF_TOTAL_CHARS: usize = 4000;

pub(crate) async fn load_task(task_id: &str, ctx: &SlotContext) -> Result<Task, String> {
    ctx.load_task(task_id).await
}

pub(crate) fn default_target_branch(_task: &Task) -> String {
    "main".to_string()
}

pub(crate) fn format_command_details(details: &str) -> String {
    smart_truncate(details, 2000)
}

pub(crate) fn conflict_context_for_dispatch(
    _task_id: &str,
    _ctx: &SlotContext,
) -> Option<crate::MergeConflictMetadata> {
    None
}

pub(crate) async fn initial_user_message_for_task(
    task_id: &str,
    _task: &Task,
    ctx: &SlotContext,
) -> String {
    ctx.callbacks.initial_user_message(task_id, ctx).await
}

pub(crate) async fn recent_feedback(_task_id: &str, _ctx: &SlotContext) -> Option<String> {
    None
}

pub(crate) async fn pr_review_feedback_context(
    _task_id: &str,
    _ctx: &SlotContext,
) -> Option<String> {
    None
}

pub(crate) fn parse_conflict_metadata(_task: &Task) -> Option<crate::MergeConflictMetadata> {
    None
}

pub(crate) fn extract_worker_context(_task_id: &str, _ctx: &SlotContext) -> Option<String> {
    None
}

pub(crate) async fn raw_ci_feedback_in_cycle(_task_id: &str, _ctx: &SlotContext) -> Option<String> {
    None
}

pub(crate) fn budget_combined_sections(
    sections: &[(String, usize)],
    _total_budget: usize,
) -> Vec<(String, String)> {
    sections
        .iter()
        .map(|(s, _)| (s.clone(), s.clone()))
        .collect()
}

pub(crate) fn runtime_env_diagnostics() -> String {
    String::new()
}

pub(crate) fn runtime_fs_diagnostics() -> String {
    String::new()
}

pub(crate) fn log_snippet(_label: &str, _text: &str, _max_chars: usize) -> String {
    String::new()
}
