//! `djinn-agent` side of the Phase 2 PR 2 supervisor split.
//!
//! The orchestration body (`TaskRunSupervisor`, `SupervisorServices`,
//! `StageOutcome`, etc.) lives in the `djinn-supervisor` crate. The
//! per-stage executor and the PR-open body remain here in `djinn-agent`
//! because they reach deeply into the lifecycle helpers, role trait impls,
//! `task_merge`, the reply loop, and `AgentContext`.
//!
//! `djinn-supervisor` exposes three `dyn Fn` seams on `SupervisorServices`:
//! `load_task_fn`, `execute_stage_fn`, `open_pr_fn`. This module provides
//! the bodies those closures forward into, plus [`SupervisorCallbackContext`]
//! — the captured per-task-run state (`AgentContext`, cancellation,
//! optional provider override) the closures need on each invocation.
//!
//! The construction site lives in
//! [`crate::actors::slot::supervisor_runner::run_supervisor_dispatch`].

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use djinn_provider::provider::LlmProvider;

// Facade: disposition logic lives in `djinn-coordinator`.
pub(crate) mod disposition {
    pub use djinn_coordinator::supervisor_impl::disposition::*;
}

pub(crate) mod pr;
#[cfg(test)]
mod pr_close;
pub(crate) mod stage;

/// Non-PR reusable live-mover API surface for post-run and orphan-task checks
/// (e.g. the future `vnwi` doctor orphan-task check). The crate-internal
/// `supervisor_impl` module root is the canonical call site for callers that
/// must not depend on PR-open-specific code — these re-exports are what makes
/// the API reachable without importing `supervisor_impl::pr`.
#[allow(unused_imports)]
pub(crate) use disposition::{
    LiveMoverEvidence, LiveMoverReason, LiveMoverSummary, has_live_mover, live_mover_reasons,
    live_mover_summary, summarize_live_mover,
};
pub(crate) use pr::supervisor_pr_open;
pub(crate) use stage::execute_stage;

/// Extra state captured when `supervisor_runner` builds the closures that
/// populate `djinn_supervisor::SupervisorServices`.
///
/// The supervisor body only sees `SupervisorServices` (the concrete struct
/// from `djinn-supervisor`); this context hops in through the closure
/// environments so `execute_stage` / `supervisor_pr_open` still have
/// everything they used to receive via the old `SupervisorServices::new`.
#[derive(Clone)]
pub(crate) struct SupervisorCallbackContext {
    pub agent_context: AgentContext,
    pub cancel: CancellationToken,
    /// Injected `LlmProvider`. This is THE production in-Pod worker path
    /// (`djinn-agent-worker` builds the provider from a Secret-mounted
    /// credential and passes it here) as well as the integration-test stub
    /// seam. When set, `execute_stage` skips `resolve_model_and_credential`.
    /// Host dispatch leaves this `None` and resolves the credential itself.
    pub provider_override: Option<Arc<dyn LlmProvider>>,
    /// Billing signal `(CostBasisHint, BillingSource)` pre-derived by the
    /// caller. The worker path derives it from the `SerializableCredential`
    /// kind (host resolution is unavailable in-Pod) and supplies it here so
    /// the session books the correct `cost_basis`. `None` for the host path
    /// (derived from the resolved credential) and the supervisor-stub test.
    pub billing_signal: Option<(
        djinn_supervisor::services::CostBasisHint,
        djinn_supervisor::services::BillingSource,
    )>,
}
