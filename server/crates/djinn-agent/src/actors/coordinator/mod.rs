// CoordinatorActor — 1x global, orchestrates phase execution and task dispatch.
//
// Ryhl hand-rolled actor pattern (AGENT-01):
//   - `CoordinatorHandle` (mpsc sender) is the public API.
//   - `CoordinatorActor` (mpsc receiver) runs in a dedicated tokio task.
//
// Main loop (AGENT-07): tokio::select! over four arms:
//   1. CancellationToken — graceful shutdown.
//   2. mpsc message channel — API calls from MCP tools.
//   3. broadcast::Receiver<DjinnEventEnvelope> — react to open-task events.
//   4. 30-second Interval tick — stuck detection safety net (AGENT-08).
//
// These imports are used by child submodules (dispatch, health, wave, rules,
// pr_poller, prompt_eval) which use `use super::*;` to access the coordinator's
// shared vocabulary.  In non-test builds some may appear unused at _this_ level.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant as StdInstant};

use crate::actors::slot::{PoolError, SlotPoolHandle};
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::{ActivityQuery, ReadyQuery, TaskRepository};
// These additional imports are only used by `#[cfg(test)]` blocks in child
// submodules (rules, health, prompt_eval, etc.) via `use super::*;`.
#[cfg(test)]
use djinn_core::events::DjinnEventEnvelope;
#[cfg(test)]
use djinn_db::Database;
#[cfg(test)]
use djinn_provider::catalog::CatalogService;

// ─── Submodules ──────────────────────────────────────────────────────────────

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
// Refinement workflow skeleton — types are `pub` for future integration by
// coordinator dispatch/actor modules, but nothing in the crate consumes them
// yet.  Allow dead_code until the full loop is wired into the coordinator.
#[allow(dead_code)]
pub(crate) mod refinement;
// Refinement tribunal dispatch orchestration — drives the advocate/adversary/
// judge phase loop.
mod refinement_dispatch;
pub(crate) mod rules;
mod types;
mod wave;

// Re-export public types so the external API is unchanged.
pub use handle::CoordinatorHandle;
pub use types::{
    AutoMergeTracker, BackgroundWorkTracker, BreakerDebugEntry, CoordinatorDebugSnapshot,
    CoordinatorDeps, CoordinatorError, CoordinatorStatus, DebugCooldown, DebugDispatchState,
    DebugFailureStreak, DebugInflightEntry, DebugSlot, DebugTotals, DispatchPauseView,
    PrCleanupConfig,
};

// Re-export internal types for sibling submodules that use `use super::*;`.
use actor::CoordinatorActor;
use types::*;

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
