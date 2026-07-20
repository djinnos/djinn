// Slot Facade (p6i4): re-exports canonical djinn-slot types, retains host-only
// modules that wire AgentContext, and provides thin adapter wrappers.

pub use djinn_orchestration_types::slot::{
    MERGE_CONFLICT_PREFIX, MergeConflictMetadata, ModelSlotConfig, SlotInfo, SlotPoolConfig,
    SlotState,
};

pub use djinn_slot::SlotEvent;

pub use djinn_slot::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, run_memory_enrichment,
    run_memory_enrichment_with_db,
};

pub use djinn_slot::run_llm_extraction;

mod actor; // HOST-ONLY: slot actor + handle
pub(crate) mod adapter; // SHARED: AgentContext → SlotContext construction helpers
mod commands; // THIN SHIM: SlotCommand/SlotError re-export
pub(crate) mod finalize_handlers; // THIN SHIM: finalize handler adapters
pub mod helpers; // HOST-ONLY: provider resolution, feedback, code-context
pub(crate) mod host_callbacks; // HOST-ONLY: dispatch callback adapter
pub(crate) mod lifecycle; // HOST-ONLY: per-stage lifecycle helpers
mod pool; // HOST-ONLY: slot pool, handle, factory
pub(crate) mod reply_loop; // THIN SHIM: reply-loop facade adapter
pub(crate) mod session_extraction; // THIN SHIM: extraction backfill adapter
mod supervisor_runner; // HOST-ONLY: host-side dispatch logic

pub use actor::*;
pub use adapter::resolve_final_verification_for_task_run;
pub use commands::{SlotCommand, SlotError};
pub use helpers::*;
pub use pool::*;
