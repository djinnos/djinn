// ─── Re-exports from djinn-orchestration-types ─────────────────────────────
// Shared slot DTOs and config types are now owned by the orchestration-types
// crate so the slot side can use them without importing coordinator internals.

pub use djinn_orchestration_types::slot::{
    MERGE_CONFLICT_PREFIX, MergeConflictMetadata, ModelSlotConfig, SlotInfo, SlotPoolConfig,
    SlotState,
};

// ─── SlotEvent (unified: re-export from djinn-slot) ────────────────────────
// Phase 5 + hfhw cutover: the canonical SlotEvent enum now lives in
// `djinn-slot`.  Re-export it here so `djinn_agent::actors::slot::SlotEvent`
// and `djinn_slot::SlotEvent` name the same type (not a duplicate).

pub use djinn_slot::SlotEvent;

// ─── Memory enrichment re-exports (delegated to djinn-slot) ────────────────
// hfhw cutover: the memory enrichment types and public entry points are now
// owned by `djinn-slot`.  Re-export them here so external callers
// (`djinn-server`'s `mcp_bridge`, `djinn-control-plane` test-support) continue
// to compile under `djinn_agent::actors::slot::*` paths.
//
// The old local production implementation of `run_memory_enrichment_inner` has
// been removed; the public API delegates to `djinn-slot`'s implementation.

pub use djinn_slot::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport,
    run_memory_enrichment, run_memory_enrichment_with_db,
};

// ─── Submodules ────────────────────────────────────────────────────────────

mod actor;
mod commands;
pub(crate) mod finalize_handlers;
pub mod helpers;
// Task #8: `lifecycle` is now a thin module owning only the per-stage helpers
// (setup / model / mcp / prompt-context / teardown / retry) reused by the
// supervisor's `execute_stage`.  The legacy `run_task_lifecycle` entry point
// and worktree orchestration have been deleted.
//
// NOTE: the lifecycle helpers (`build_prompt_context`, etc.) remain in
// `djinn-agent` because `supervisor_impl::stage::execute_stage` calls them
// with `AgentContext`.  They are NOT duplicated production implementations —
// the djinn-slot equivalents are thin host-callback delegates.  These agent-
// side helpers will become unreachable once `SlotHostCallbacks` is implemented
// on the agent and `execute_stage` is ported to use `SlotContext`.
pub(crate) mod lifecycle;
pub(crate) mod llm_extraction;
mod pool;
pub(crate) mod reply_loop;
#[cfg(test)]
mod reply_loop_tests;
// hfhw cutover: `session_extraction` is a thin adapter that converts
// `AgentContext` → `SlotContext` and delegates to `djinn_slot::run_extraction_backfill`.
// The full structural extraction implementation now lives in `djinn-slot`.
pub(crate) mod session_extraction;
// NOTE: `supervisor_runner` remains in `djinn-agent` because the slot actor
// spawns it with `AgentContext`.  The djinn-slot equivalent delegates to
// `SlotHostCallbacks::run_task_dispatch`.  This will become unreachable once
// the agent-side pool/actor are ported to use `SlotContext`.
mod supervisor_runner;

pub use actor::*;
pub(crate) use commands::*;
pub use helpers::*;
pub use pool::*;

#[cfg(test)]
mod helpers_tests;

#[cfg(test)]
mod llm_extraction_tests;
