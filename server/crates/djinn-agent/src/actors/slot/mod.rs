// ─── Re-exports from djinn-orchestration-types ─────────────────────────────
// Shared slot DTOs and config types are now owned by the orchestration-types
// crate so the slot side can use them without importing coordinator internals.

pub use djinn_orchestration_types::slot::{
    MERGE_CONFLICT_PREFIX, MergeConflictMetadata, ModelSlotConfig, SlotInfo, SlotPoolConfig,
    SlotState,
};

// ─── Slot types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SlotEvent {
    /// Slot finished its task (success or failure) and is free for reassignment.
    Free {
        slot_id: usize,
        model_id: String,
        task_id: String,
    },
    /// Slot's task was killed by external request.
    Killed {
        slot_id: usize,
        model_id: String,
        task_id: String,
    },
}

// ─── Submodules ───────────────────────────────────────────────────────────────

mod actor;
mod commands;
pub(crate) mod finalize_handlers;
pub mod helpers;
// Task #8: `lifecycle` is now a thin module owning only the per-stage helpers
// (setup / model / mcp / prompt-context / teardown / retry) reused by the
// supervisor's `execute_stage`.  The legacy `run_task_lifecycle` entry point
// and worktree orchestration have been deleted.
pub(crate) mod lifecycle;
pub(crate) mod llm_extraction;
pub(crate) mod memory_enrichment;
/// Re-export the public memory-enrichment entry point so non-agent crates
/// (e.g. `djinn-server`'s `mcp_bridge`) can trigger the pass without
/// depending on `djinn_agent::actors::slot` internals.
///
/// The trigger (`memory_run_enrichment` MCP tool) is intentionally a thin
/// admin/operator surface — see `djinn-control-plane::tools::memory_tools::run_enrichment`.
pub use memory_enrichment::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, run_memory_enrichment,
    run_memory_enrichment_with_db,
};
mod pool;
pub(crate) mod reply_loop;
#[cfg(test)]
mod reply_loop_tests;
pub(crate) mod session_extraction;
pub(crate) mod supervisor_runner;

pub use actor::*;
pub(crate) use commands::*;
pub use helpers::*;
pub use pool::*;

#[cfg(test)]
mod helpers_tests;

#[cfg(test)]
mod llm_extraction_tests;
