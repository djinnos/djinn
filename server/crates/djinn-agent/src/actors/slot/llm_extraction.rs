// ─── p6i4 cutover: LLM extraction delegated to djinn-slot ────────────────
//
// The canonical LLM-powered session knowledge extraction implementation now
// lives in `djinn_slot::llm_extraction`.  This module is retained only as an
// agent compatibility shim for tests and any agent-internal callers that still
// pass `AgentContext` instead of the canonical `SlotContext`.
//
// Production callers should prefer the facade-level `djinn_slot` re-export in
// `super::mod.rs` or call `djinn_slot::run_llm_extraction` directly.  No prompt,
// extraction, deduplication, admission-gate, or note-persistence behavior is
// implemented here.

#[cfg(test)]
use crate::context::AgentContext;

/// Agent-compatible wrapper around `djinn_slot::run_llm_extraction`.
///
/// Converts `AgentContext` → `SlotContext` and delegates to the canonical
/// djinn-slot implementation.
#[cfg(test)]
#[allow(dead_code)] // retained for agent facade compatibility tests; canonical home is djinn-slot
pub(crate) async fn run_llm_extraction(
    session_id: String,
    taxonomy: super::session_extraction::SessionTaxonomy,
    app_state: AgentContext,
) {
    let slot_ctx = super::session_extraction::agent_to_slot_context(&app_state);
    djinn_slot::run_llm_extraction(session_id, taxonomy, slot_ctx).await;
}

/// Test-support adapter for injecting a fake provider while exercising the
/// canonical djinn-slot LLM extraction implementation.
#[cfg(test)]
#[allow(dead_code)] // retained for agent facade compatibility tests; canonical home is djinn-slot
pub(crate) async fn run_llm_extraction_with_provider(
    session_id: String,
    taxonomy: super::session_extraction::SessionTaxonomy,
    app_state: AgentContext,
    provider: std::sync::Arc<dyn djinn_provider::provider::LlmProvider>,
) {
    let slot_ctx = super::session_extraction::agent_to_slot_context(&app_state);
    djinn_slot::llm_extraction::run_llm_extraction_with_provider(
        session_id, taxonomy, slot_ctx, provider,
    )
    .await;
}

/// Test-support adapter for injecting both a fake provider and deterministic
/// novelty candidates while exercising the canonical djinn-slot implementation.
#[cfg(test)]
#[allow(dead_code)] // retained for agent facade compatibility tests; canonical home is djinn-slot
pub(crate) async fn run_llm_extraction_with_provider_and_candidate_lookup(
    session_id: String,
    taxonomy: super::session_extraction::SessionTaxonomy,
    app_state: AgentContext,
    provider: std::sync::Arc<dyn djinn_provider::provider::LlmProvider>,
    candidate_lookup_override: djinn_slot::llm_extraction::CandidateLookupOverride,
) {
    let slot_ctx = super::session_extraction::agent_to_slot_context(&app_state);
    djinn_slot::llm_extraction::run_llm_extraction_with_provider_and_candidate_lookup(
        session_id,
        taxonomy,
        slot_ctx,
        provider,
        candidate_lookup_override,
    )
    .await;
}
