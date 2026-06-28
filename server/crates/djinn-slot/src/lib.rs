#![allow(clippy::disallowed_methods)] // TODO(70y0): temporary; remove after wall-clock migration
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

// ─── Public host seam ───────────────────────────────────────────────────────

pub mod host;
pub use host::{KnowledgeBranchTarget, SlotContext, SlotHostCallbacks};

// ─── Supporting types extracted from djinn-agent ────────────────────────────

pub mod output_parser;
pub mod roles_support;
pub mod truncate;

// ─── Re-exports from djinn-orchestration-types ─────────────────────────────

pub use djinn_orchestration_types::slot::{
    MERGE_CONFLICT_PREFIX, MergeConflictMetadata, ModelSlotConfig, SlotInfo, SlotPoolConfig,
    SlotState,
};

// ─── Slot modules ───────────────────────────────────────────────────────────

mod actor;
mod commands;
pub(crate) mod finalize_handlers;
pub mod finalize_types;
pub mod helpers;
pub(crate) mod lifecycle;
pub(crate) mod llm_extraction;
pub(crate) mod memory_enrichment;
mod pool;
pub(crate) mod reply_loop;
pub(crate) mod session_extraction;
mod supervisor_runner;

// ─── Test modules ───────────────────────────────────────────────────────────

#[cfg(test)]
mod helpers_tests;
#[cfg(test)]
mod llm_extraction_tests;
// reply_loop_tests.rs: disabled — tests reference `crate::context::AgentContext`,
// the old ReplyLoopContext struct (with many fields removed during extraction),
// and `crate::test_helpers::test_services` which no longer exists.
// These tests exercise the full reply loop implementation which is still owned
// by djinn-agent. Re-enable after the reply loop is fully extracted to djinn-slot.
// #[cfg(test)]
// mod reply_loop_tests;
#[cfg(test)]
pub(crate) mod test_helpers;

// ─── Public re-exports ──────────────────────────────────────────────────────

pub use actor::*;
pub(crate) use commands::*;
pub use helpers::*;
pub use pool::*;

// ─── Memory enrichment re-exports ───────────────────────────────────────────

pub use memory_enrichment::{
    EnrichmentClaim, EnrichmentEdge, EnrichmentEntity, EnrichmentReport, run_memory_enrichment,
    run_memory_enrichment_with_db,
};

// ─── Session extraction re-export ───────────────────────────────────────────

pub use session_extraction::{
    ExtractionQuality, SessionSignals, SessionTaxonomy, derive_scope_paths,
    extract_session_signals, run_extraction_backfill, run_post_session_extraction,
    run_structural_extraction,
};

// ─── Supervisor runner re-export ────────────────────────────────────────────
// hfhw cutover: expose `run_supervisor_dispatch` so the host (djinn-agent)
// can call through the djinn-slot pathway from `actor.rs`.

pub use supervisor_runner::run_supervisor_dispatch;

// ─── SlotEvent ──────────────────────────────────────────────────────────────

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
