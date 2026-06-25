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
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, run_memory_enrichment,
    run_memory_enrichment_with_db,
};

// ─── Submodules ────────────────────────────────────────────────────────────

mod actor;
mod commands;
pub(crate) mod finalize_handlers;
pub mod helpers;
// hfhw cutover: host callback implementation for the djinn-slot dispatch pathway.
// `AgentDispatchCallbacks` implements `SlotHostCallbacks::run_task_dispatch`
// by delegating to `supervisor_runner::dispatch_task_runtime`.
pub(crate) mod host_callbacks;
// Task #8: `lifecycle` is now a thin module owning only the per-stage helpers
// (setup / model / mcp / prompt-context / teardown / retry) reused by the
// supervisor's `execute_stage`.  The legacy `run_task_lifecycle` entry point
// and worktree orchestration have been deleted.
//
// hfhw cutover: the lifecycle module owns per-stage helpers including
// `assemble_prompt_context` (the prompt assembly logic).  The old
// `build_prompt_context` stub has been removed — `stage::execute_stage`
// calls `assemble_prompt_context` directly with `AgentContext` through the
// host callback dispatch path.
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
// hfhw cutover: `supervisor_runner` contains the host-side dispatch logic
// (`dispatch_task_runtime`) called through `host_callbacks::AgentDispatchCallbacks`.
// The old `run_supervisor_dispatch` stub has been removed — the slot actor
// dispatches through `djinn_slot::run_supervisor_dispatch` →
// `SlotHostCallbacks::run_task_dispatch` → `dispatch_task_runtime`.
mod supervisor_runner;

pub use actor::*;
pub(crate) use commands::*;
pub use helpers::*;
pub use pool::*;

#[cfg(test)]
mod helpers_tests;

#[cfg(test)]
mod llm_extraction_tests;
