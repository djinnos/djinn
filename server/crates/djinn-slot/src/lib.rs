// Test-only slot fixtures intentionally use unwrap/expect/panic and real
// clocks for assertion readability; production targets deny these lints via
// Cargo.toml plus the non-test module-scoped allowances below.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::disallowed_methods
    )
)]
//! # djinn-slot
//!
//! Production slot management crate extracted from `djinn-agent`.
//!
//! This crate owns the slot actor, slot pool, and slot lifecycle infrastructure.
//! Host integration (database, event bus, coordinator, MCP tools, etc.) is
//! abstracted through the [`SlotContext`] concrete host context and the
//! [`SlotHostCallbacks`] trait, with the canonical implementation in
//! `djinn_agent::context::AgentContext`.

#![recursion_limit = "256"]
// Temporary: many internal modules are extracted but not yet fully wired into
// the public facade. Allow dead_code so the crate compiles while the extraction
// is in progress. Remove once all modules are connected.
#![allow(dead_code)]

pub mod host;
pub use host::{KnowledgeBranchTarget, SlotContext, SlotHostCallbacks, SlotToolDispatcher};

pub mod output_parser;
pub mod roles_support;
pub mod truncate;

pub use djinn_orchestration_types::slot::{
    MERGE_CONFLICT_PREFIX, MergeConflictMetadata, ModelSlotConfig, SlotInfo, SlotPoolConfig,
    SlotState,
};

mod actor;
pub mod attempt_lifecycle;
pub mod commands;
pub mod extraction_replay_eval;
pub mod final_verification;
pub mod finalize_handlers;
pub mod finalize_types;
pub mod helpers;
pub mod lifecycle;
pub mod llm_extraction;
pub mod memory_enrichment;
pub mod pool;
pub mod reply_loop;
pub mod session_extraction;
mod supervisor_runner;

#[cfg(test)]
mod finalize_handlers_fingerprint_tests;
#[cfg(test)]
mod finalize_handlers_tests;
#[cfg(test)]
mod helpers_tests;
#[cfg(test)]
mod llm_extraction_tests;
#[cfg(test)]
mod reply_loop_tests;
#[cfg(test)]
pub(crate) mod test_helpers;

pub use actor::*;
// Public re-exports from `commands` so callers can use
// `djinn_slot::SlotCommand`, `djinn_slot::SlotError`, `djinn_slot::log_commands_run_event`.
pub use commands::{SlotCommand, SlotError, log_commands_run_event};
// Public re-exports from `finalize_handlers` so callers can use
// `djinn_slot::process_finalize_payload`, `djinn_slot::handle_budget_park`,
// `djinn_slot::apply_ac_verdicts`.
pub use finalize_handlers::{apply_ac_verdicts, handle_budget_park, process_finalize_payload};
pub use helpers::*;
pub use pool::*;

pub use llm_extraction::{
    TerminalExtractionContext, TerminalExtractionOutcome, TerminalReviewDecision,
    run_llm_extraction, run_llm_extraction_with_terminal_context,
};
#[cfg(any(test, feature = "test-support"))]
pub use llm_extraction::{
    capture_llm_extraction_replay, run_llm_extraction_with_provider,
    run_llm_extraction_with_provider_and_candidate_lookup,
};

pub use memory_enrichment::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, run_memory_enrichment,
    run_memory_enrichment_with_db,
};

pub use session_extraction::{
    ExtractionQuality, SessionSignals, SessionTaxonomy, derive_scope_paths,
    extract_session_signals, run_extraction_backfill, run_post_session_extraction,
    run_structural_extraction,
};

// hfhw cutover: expose `run_supervisor_dispatch` so the host (djinn-agent)
// can call through the djinn-slot pathway from `actor.rs`.

pub use supervisor_runner::run_supervisor_dispatch;

// The canonical reply loop implementation now lives in djinn-slot.
// Re-export the public surface so callers can use
// `djinn_slot::reply_loop::{ReplyLoopContext, run_reply_loop}`.

pub use reply_loop::{CompactionCriticalSection, ReplyLoopContext, run_reply_loop};

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
