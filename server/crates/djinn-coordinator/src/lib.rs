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

pub mod audit_sampler;
pub mod build_admission;
pub mod build_admission_handoff;
pub mod build_admission_inventory;
/// Durable v1 lease service; v0 admission remains rollout authority.
pub mod build_lease;
pub mod cargo_warm_base_gc;
pub(crate) mod ci_preflight_gate;
pub mod ci_reproduction;
pub mod context;
pub mod disk_admission;
pub mod dispatch_pause;
pub mod doctor;
pub mod environment;
pub mod events;
pub mod file_time;
pub mod github_error_render;
/// Adapter that gives graph warming the same v1 FIFO service as task consumers.
pub mod graph_warm_lease;
pub mod output_stash;

/// Terminalize the worker's in-flight attempt (and record a durable `reopened`
/// marker) for a supervisor-driven rework reopen that the PR poller's
/// `apply_pr_transition` does not own.
///
/// The reviewer's `task_review_reject*` and the lead's `lead_approve_conflict`
/// transitions reopen the task to a dispatchable state but historically left
/// the worker's `submitted` attempt untouched (the ylme orphan: a `submitted`
/// row with no pr_url that made the respawn guard's step-2 dedup defer forever,
/// which the orphan reaper deliberately skipped).  Called from the transition
/// apply layer (`DirectServices::transition_task`, the single chokepoint for
/// both in-process and RPC-hosted supervisor transitions) after the board
/// transition succeeds; a no-op for non-rework actions.  Best-effort.
pub async fn record_supervisor_rework_reopen(
    db: &djinn_db::Database,
    task_id: &str,
    action: &djinn_core::models::TransitionAction,
    reason: Option<&str>,
) {
    use djinn_core::models::TransitionAction;
    let is_supervisor_rework = matches!(
        action,
        TransitionAction::TaskReviewReject
            | TransitionAction::TaskReviewRejectStale
            | TransitionAction::TaskReviewRejectConflict
            | TransitionAction::LeadApproveConflict
    );
    if !is_supervisor_rework {
        return;
    }
    // Best-effort: read the task's pr_url so the terminalized attempt keeps its
    // PR linkage when one exists (internal task-review rejects often have none).
    let pr_url =
        match djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .get(task_id)
            .await
        {
            Ok(Some(t)) => t.pr_url,
            _ => None,
        };
    crate::dispatch::attempt_lifecycle::record_rework_reopen(
        db,
        task_id,
        "worker",
        pr_url.as_deref(),
        reason,
        None,
    )
    .await;
}

pub mod resource_monitor;
pub mod roles;
pub mod run_dir_reconcile;
pub mod supervisor_impl;
pub mod task_merge;
pub(crate) mod tripwires;
pub(crate) mod truncate;

// ─── Coordinator actor tree (was actors::coordinator in djinn-agent) ──────

mod actor;
mod consolidation;
pub mod dispatch;
mod evidence;
mod evidence_lifecycle_state;
pub mod handle;
mod health;
pub mod messages;
pub mod pr_poller;
mod recover_terminal_linked_spike_evidence;
mod reentrance;
#[allow(dead_code)]
pub(crate) mod refinement;
pub(crate) mod refinement_dispatch;
#[cfg(test)]
mod refinement_e2e_evidence_regression_tests;
mod refinement_lint_evidence;
mod refinement_objections;
mod refinement_outcome;
mod refinement_recovery;
pub mod rules;
mod tripwire_hold_release;
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
    CheckpointSafetyScanMetadata, ControlledExitPreservationAction, DurableProgressDetectionMode,
    DurableProgressNoResetReason, DurableProgressResetReason, DurableProgressRolloutConfig,
    ModelRotationLifecycleConfig, ModelRotationLifecycleMetadata, ModelRotationReason,
    NoProgressCommandState, NoProgressControlledExitDecision, NoProgressEnforcementMode,
    NoProgressThresholdConfig, PreservationFailurePolicy, PreservationGateResult,
    PreservationOutcome, ResumeLifecycleConfig, ResumeLifecycleEnvFlag, ResumeLifecycleMetadata,
    ResumeSelectionReason, SlowExtensionConfig, WorkerLifecycleConfig, WorkerLifecycleMetadata,
    decide_controlled_exit_preservation_action, evaluate_no_progress_controlled_exit,
};

// Re-export orchestration-types debug DTOs so djinn-agent can re-export from here.
pub use djinn_orchestration_types::coordinator::PR_REVIEW_FEEDBACK_EVENT;

// ─── Test modules ────────────────────────────────────────────────────────

#[cfg(test)]
mod build_admission_integration_tests;
#[cfg(test)]
mod build_admission_inventory_tests;
#[cfg(test)]
mod build_lease_integration_tests;
#[cfg(test)]
pub(crate) mod test_helpers;
#[cfg(test)]
mod tests;
