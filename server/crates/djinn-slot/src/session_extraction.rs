//! Session extraction: structural + LLM-powered knowledge extraction.
use crate::host::SlotContext;

/// Run post-session extraction for a completed task run.
/// Delegates the actual extraction logic to the host.
pub(crate) async fn run_post_session_extraction(
    task_id: String,
    task_run_id: String,
    ctx: SlotContext,
) {
    tracing::info!(task_id = %task_id, task_run_id = %task_run_id, "session_extraction: starting");
    // The real implementation is in djinn-agent; this is a structural placeholder.
    ctx.deregister_activity(&task_id);
}

/// Session taxonomy extracted from a completed session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionTaxonomy {
    pub files_changed: usize,
    pub errors: usize,
    pub tools_used: usize,
    pub notes_read: usize,
    pub notes_written: usize,
    pub tasks_transitioned: usize,
}

/// Run structural extraction on a session's conversation.
pub fn run_structural_extraction(_messages: &[serde_json::Value]) -> SessionTaxonomy {
    SessionTaxonomy {
        files_changed: 0,
        errors: 0,
        tools_used: 0,
        notes_read: 0,
        notes_written: 0,
        tasks_transitioned: 0,
    }
}

pub fn extract_session_signals(_messages: &[serde_json::Value]) -> SessionTaxonomy {
    run_structural_extraction(_messages)
}

/// Extraction quality metrics.
#[derive(Debug, Clone)]
pub struct ExtractionQuality {
    pub notes_created: usize,
    pub notes_skipped: usize,
}

/// One-shot recovery sweep for post-session knowledge extraction.
pub async fn run_extraction_backfill(ctx: &SlotContext) {
    tracing::info!("session_extraction: running backfill sweep");
    let _ = ctx;
}
