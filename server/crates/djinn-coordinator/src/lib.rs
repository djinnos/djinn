//! # djinn-coordinator
//!
//! Coordinator actor crate extracted from `djinn-agent`.
//!
//! Owns the coordinator actor loop, dispatch logic, health sweeps,
//! PR poller, doctor checks, and the supervisor disposition/live-mover
//! evaluation surface.  Host integration (AgentContext, task merge,
//! output stash, etc.) is consumed through crate-internal compatibility
//! shims that mirror the original `djinn-agent` module definitions.

#![allow(dead_code)]
#![allow(unused_imports)]

// These imports are used by child submodules (dispatch, health, wave, rules,
// pr_poller, prompt_eval) which use `use super::*;` to access the coordinator's
// shared vocabulary.  In non-test builds some may appear unused at _this_ level.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::{ActivityQuery, ReadyQuery, TaskRepository};
use djinn_slot::{PoolError, SlotPoolHandle};

#[cfg(test)]
use djinn_core::events::DjinnEventEnvelope;
#[cfg(test)]
use djinn_db::Database;
#[cfg(test)]
use djinn_provider::catalog::CatalogService;

// ─── Compatibility modules (types previously in djinn-agent) ────────────────

pub mod context;
mod dispatch_pause;
mod environment;
mod events;
mod file_time;
mod github_error_render;
mod output_stash;
mod resource_monitor;
mod roles_compat;
mod task_merge;
#[cfg(test)]
pub mod test_helpers;

// ─── Supervisor pieces (moved from djinn-agent/src/supervisor_impl/) ────────

pub mod supervisor_impl;

// ─── Doctor (moved from djinn-agent/src/doctor/) ────────────────────────────

pub mod doctor;

// ─── Coordinator modules ────────────────────────────────────────────────────

mod actor;
mod consolidation;
mod dispatch;
mod evidence;
mod handle;
mod health;
mod messages;
pub(crate) mod pr_poller;
mod prompt_eval;
mod reentrance;
pub(crate) mod rules;
mod types;
mod wave;

// Re-export public types.
pub use handle::CoordinatorHandle;
pub use types::{
    AutoMergeFastPathState, AutoMergeTracker, BackgroundWorkTracker, BreakerDebugEntry,
    CoordinatorDebugSnapshot, CoordinatorDeps, CoordinatorError, CoordinatorStatus, DebugCooldown,
    DebugDispatchState, DebugFailureStreak, DebugInflightEntry, DebugSlot, DebugTotals,
    DispatchPauseView, PrCleanupConfig,
};

// Re-export doctor types for the public API.
pub use doctor::{LiveMoverSource, register_doctor_checks};

pub use supervisor_impl::disposition::{
    LiveMoverEvidence, LiveMoverReason, LiveMoverSummary, has_live_mover, live_mover_reasons,
    live_mover_summary, summarize_live_mover,
};

// Re-export internal types for sibling submodules that use `use super::*;`.
use actor::CoordinatorActor;
use types::*;

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
