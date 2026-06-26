//! Shared orchestration DTOs, config types, event constants, and traits
//! for the slot/coordinator boundary in `djinn-agent`.
//!
//! This crate is a **leaf** in the dependency graph: it must never depend on
//! `djinn-agent`, `djinn-slot`, `djinn-coordinator`, or non-DTO repository
//! internals.  Its purpose is to break the slot → coordinator edge by owning
//! the types that both sides need.

pub mod coordinator;
pub mod slot;
pub mod trigger;

// ─── Re-exports for flat access ────────────────────────────────────────────

pub use coordinator::{
    BackgroundWorkTracker, BreakerDebugEntry, CoordinatorDebugSnapshot, DebugCooldown,
    DebugDispatchState, DebugFailureStreak, DebugInflightEntry, DebugSlot, DebugTotals,
    DispatchPauseView, PR_REVIEW_FEEDBACK_EVENT,
};
pub use slot::{
    MERGE_CONFLICT_PREFIX, MergeConflictMetadata, ModelSlotConfig, SlotInfo, SlotPoolConfig,
    SlotState,
};
pub use trigger::CoordinatorTrigger;
