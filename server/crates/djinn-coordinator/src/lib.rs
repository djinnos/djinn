// Test-only coordinator fixtures intentionally use unwrap/expect/panic and
// real clocks for assertion readability; production targets deny these lints
// via Cargo.toml plus the non-test module-scoped wall-clock allowances below.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::disallowed_methods
    )
)]
//! # djinn-coordinator
//!
//! Production coordinator, doctor, and coordinator-owned supervisor
//! implementation extracted from `djinn-agent`.
//!
//! This crate owns the global coordinator actor, dispatch logic, PR
//! polling, health checks, doctor seed checks, and the supervisor
//! disposition layer.  It depends on `djinn-slot`, `djinn-roles`,
//! `djinn-orchestration-types`, and shared domain crates — but **never**
//! on `djinn-agent`.

// ─── Imports available to all child submodules via `use super::super::*` ──
// These mirror the module-level imports in the original
// djinn-agent/src/actors/coordinator/mod.rs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant as StdInstant;

#[cfg(test)]
use djinn_core::events::DjinnEventEnvelope;
#[cfg(test)]
use djinn_db::Database;
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::{ActivityQuery, ReadyQuery, TaskRepository};
#[cfg(test)]
use djinn_provider::catalog::CatalogService;
use djinn_slot::{PoolError, SlotPoolHandle};

// Re-export internal types for sibling submodules that use `use super::*;`.
use actor::CoordinatorActor;
use types::*;

pub(crate) mod ci_preflight_gate;
pub mod ci_reproduction;
pub mod context;
pub mod dispatch_pause;
pub mod doctor;
pub mod environment;
pub mod events;
pub mod file_time;
pub mod github_error_render;
pub mod output_stash;
pub mod resource_monitor;
pub mod roles;
pub mod supervisor_impl;
pub mod task_merge;
pub(crate) mod truncate;

// ─── Coordinator actor tree (was actors::coordinator in djinn-agent) ──────

mod actor;
mod consolidation;
pub mod dispatch;
mod evidence;
pub mod handle;
mod health;
pub mod messages;
pub mod pr_poller;
mod prompt_eval;
mod reentrance;
#[allow(dead_code)]
pub(crate) mod refinement;
mod refinement_dispatch;
mod refinement_outcome;
pub mod rules;
mod types;
mod wave;
mod worker_lifecycle;

// ─── Public re-exports (matching djinn-agent facade paths) ───────────────

pub use handle::CoordinatorHandle;
pub use types::{
    AutoMergeTracker, BackgroundWorkTracker, BreakerDebugEntry, CoordinatorDebugSnapshot,
    CoordinatorDeps, CoordinatorError, CoordinatorStatus, DebugCooldown, DebugDispatchState,
    DebugFailureStreak, DebugInflightEntry, DebugSlot, DebugTotals, DispatchPauseView,
    PrCleanupConfig,
};
pub use worker_lifecycle::{
    AutoSubmitLifecycleConfig, AutoSubmitLifecycleMetadata, AutoSubmitSkipReason,
    CheckpointLifecycleConfig, CheckpointLifecycleMetadata, CheckpointRequestReason,
    CheckpointSafetyScanMetadata, DurableProgressDetectionMode, DurableProgressNoResetReason,
    DurableProgressResetReason, DurableProgressRolloutConfig, ModelRotationLifecycleConfig,
    ModelRotationLifecycleMetadata, ModelRotationReason, NoProgressEnforcementMode,
    NoProgressThresholdConfig, PreservationFailurePolicy, PreservationGateResult,
    PreservationOutcome, ResumeLifecycleConfig, ResumeLifecycleMetadata, ResumeSelectionReason,
    WorkerLifecycleConfig, WorkerLifecycleMetadata,
};

// Re-export orchestration-types debug DTOs so djinn-agent can re-export from here.
pub use djinn_orchestration_types::coordinator::PR_REVIEW_FEEDBACK_EVENT;

// ─── Test modules ────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod test_helpers;
#[cfg(test)]
mod tests;
