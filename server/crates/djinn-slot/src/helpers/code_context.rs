//! Code-graph context building (stub).
use crate::host::SlotContext;
use crate::truncate::smart_truncate;

pub(crate) fn is_role_auto_code_context_enabled(role_name: &str) -> bool {
    let env_val = std::env::var("DJINN_AUTO_CODE_CONTEXT_ROLES").unwrap_or_default();
    env_val.split(',').any(|r| r.trim() == role_name)
}

pub(crate) async fn build_role_code_graph_context(
    _role_name: &str,
    _worktree_path: &str,
    _ctx: &SlotContext,
) -> Option<String> {
    None
}

pub(crate) fn derive_task_scope_paths(_worktree_path: &str, _task_id: &str) -> Vec<String> {
    Vec::new()
}

pub(crate) fn format_knowledge_notes(notes: &[&str], max_chars: usize) -> String {
    let combined = notes.join("\n---\n");
    smart_truncate(&combined, max_chars)
}
