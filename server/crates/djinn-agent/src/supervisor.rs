//! Thin re-export shim for Phase 2.
//!
//! The supervisor body was moved into its own crate
//! [`djinn-supervisor`](../djinn_supervisor/index.html) so the future
//! `djinn-agent-worker` binary can link against the orchestration loop
//! without pulling in `AgentContext`, the coordinator, the actor
//! framework, or the reply loop.
//!
//! This file re-exports every public symbol from that crate under the old
//! `djinn_agent::supervisor::*` paths so existing consumers keep compiling
//! unchanged.  The per-stage executor and the PR-open body still live
//! in-tree — see [`crate::supervisor_impl`] — and they are reached via the
//! [`crate::direct_services::DirectServices`] impl of
//! [`djinn_supervisor::SupervisorServices`].
//!
//! ## PR 3: `SupervisorServices` is now a trait
//!
//! PR 2 kept a concrete struct-with-callbacks `SupervisorServices`.  PR 3
//! swapped that for the object-safe trait in `djinn-supervisor` and split
//! the production impl into [`crate::direct_services::DirectServices`].
//! The free functions below are now 3-line constructors returning
//! `Arc<dyn SupervisorServices>` — the supervisor's dispatch shape.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

// Re-export every supervisor symbol.  Consumers that imported
// `djinn_agent::supervisor::{TaskRunSupervisor, SupervisorServices,
// SupervisorError, StageOutcome, StageError, TaskRunSpec, TaskRunOutcome,
// TaskRunReport, RoleKind, SupervisorFlow, trigger_as_str, role_sequence}`
// keep resolving through this shim.
pub use djinn_supervisor::*;

use crate::context::AgentContext;
use crate::direct_services::DirectServices;
use djinn_provider::provider::LlmProvider;

/// Build a `SupervisorServices` pre-wired with the in-tree `djinn-agent`
/// lifecycle bodies.
///
/// Returns `Arc<dyn SupervisorServices>` — the supervisor holds services
/// behind a trait object so the same `Arc` plumbing can hand them to a
/// `SessionRuntime` on the host side once PR 4/5 lands.
pub fn services_for_agent_context(
    agent_context: AgentContext,
    cancel: CancellationToken,
) -> Arc<dyn SupervisorServices> {
    Arc::new(DirectServices::new(agent_context, cancel))
}

/// Same as [`services_for_agent_context`] but installs a test-only
/// [`LlmProvider`] override on the stage executor, bypassing the catalog /
/// vault credential lookup inside `execute_stage`.
pub fn services_for_agent_context_with_provider_override(
    agent_context: AgentContext,
    cancel: CancellationToken,
    provider: Arc<dyn LlmProvider>,
) -> Arc<dyn SupervisorServices> {
    Arc::new(DirectServices::with_provider_override(
        agent_context,
        cancel,
        Some(provider),
    ))
}
