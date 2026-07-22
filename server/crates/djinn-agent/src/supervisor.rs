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

use djinn_core::models::Task;
use djinn_workspace::Workspace;

use crate::context::AgentContext;
use crate::direct_services::DirectServices;
use crate::supervisor_impl::SupervisorCallbackContext;
use djinn_provider::provider::LlmProvider;

/// Re-export the billing-signal deriver so the in-Pod worker
/// (`djinn-agent-worker`) can classify a session's `cost_basis` from the
/// Secret-mounted credential kind — the worker never runs the host's
/// `resolve_model_and_credential`.
pub use crate::supervisor_impl::stage::derive_billing_signal;

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

/// Worker-facing entrypoint into the per-stage executor.
///
/// Phase 7b of `~/.claude/plans/phase2-worker-execution-architecture.md`:
/// the in-Pod `djinn-agent-worker` constructs a per-stage `LlmProvider` from
/// the [`djinn_runtime::ResolvedCredentials`] mounted on the per-task-run
/// Secret and invokes this function to drive one role stage end-to-end. The
/// worker injects the provider via `provider_override` so
/// `supervisor_impl::stage::execute_stage` skips its catalog/vault credential
/// path entirely — the host keeps owning vault keys.
///
/// Host-bound state on `AgentContext` (DB, catalog, event_bus, activity
/// tracker) is best-effort on the worker side: see the Phase 7b design doc
/// for the panic-stub strategy. Calls that hit a missing dependency surface
/// as panics rather than silent skips so Phase 7 follow-ups can find them.
// Stage execution wires together task, services, and several context handles;
// each arg is a distinct dependency, so a bag struct adds no clarity.
#[allow(clippy::too_many_arguments)]
pub async fn worker_execute_stage(
    task: &Task,
    workspace: &Workspace,
    role_kind: djinn_supervisor::RoleKind,
    task_run_id: &str,
    spec: &djinn_supervisor::TaskRunSpec,
    agent_context: AgentContext,
    cancel: CancellationToken,
    provider: Arc<dyn LlmProvider>,
    billing_signal: Option<(
        djinn_supervisor::services::CostBasisHint,
        djinn_supervisor::services::BillingSource,
    )>,
    services: &dyn SupervisorServices,
) -> Result<djinn_supervisor::StageOutcome, djinn_supervisor::StageError> {
    let callbacks = SupervisorCallbackContext {
        agent_context,
        cancel,
        provider_override: Some(provider),
        billing_signal,
    };
    crate::supervisor_impl::execute_stage(
        task,
        workspace,
        role_kind,
        task_run_id,
        spec,
        &callbacks,
        services,
    )
    .await
}
