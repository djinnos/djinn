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
pub(crate) mod rules;
mod types;
mod wave;

// Re-export public types so the external API is unchanged.
pub use handle::CoordinatorHandle;
pub use types::{
    AutoMergeTracker, CoordinatorDeps, CoordinatorError, CoordinatorStatus, VerificationTracker,
};

// Re-export internal types for sibling submodules that use `use super::*;`.
use actor::CoordinatorActor;
use types::*;

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
