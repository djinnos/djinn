//! LLM-powered knowledge extraction (stub).
use crate::host::SlotContext;
use crate::session_extraction::SessionTaxonomy;

/// Run LLM extraction on a completed session.
pub(crate) async fn run_llm_extraction(
    _task_id: &str,
    _taxonomy: &SessionTaxonomy,
    _ctx: &SlotContext,
) -> ExtractionResult {
    ExtractionResult::default()
}

#[derive(Debug, Default)]
pub(crate) struct ExtractionResult {
    pub notes_created: usize,
}
