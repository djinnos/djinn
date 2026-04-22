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

pub(crate) mod pr;
pub(crate) mod stage;

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
    /// Test seam: integration tests inject a stubbed `LlmProvider` so the
    /// stage can run end-to-end without a real vault credential.  Production
    /// callers leave this `None`.
    pub provider_override: Option<Arc<dyn LlmProvider>>,
}
