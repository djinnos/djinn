//! The dispatch seam's view of proposal `3i92`'s post-admission resize stack.
//!
//! # Why this trait exists at all
//!
//! The machinery it fronts — the durable `build_pod_permits` relation, the
//! limits-only `pods/resize` client, the write-once ceiling capture and the
//! dispatch gate — is composed in `djinn_server::task_run_resize_bootstrap`.
//! `djinn-agent` cannot depend on the server crate (ADR-047), and the only
//! function that owns *both* the Job that was just created and the decision to
//! start a worker session lives here, in
//! [`crate::actors::slot::supervisor_runner::execute_runtime_report_phase`].
//!
//! So this is the same shape as [`crate::context::AgentContext::runtime_ops`]
//! and `repo_graph_ops`: a narrow trait declared on the agent side, with the
//! concrete implementation injected at the server boundary by
//! `AppState::agent_context()`.
//!
//! # `None` is not "disabled", it is "no server boundary"
//!
//! An `AgentContext` with no admission bridge is an off-server context: an
//! in-pod worker, or a test that never dispatches a Kubernetes Job. Those
//! contexts render no launcher sidecar, so there is no ceiling to capture and
//! no launcher container to resize. A *`resize-v2` dispatch* with no bridge is
//! a different thing entirely and the dispatch seam refuses it — see
//! `execute_runtime_report_phase`.

use async_trait::async_trait;
use djinn_launcher_protocol::LauncherAuthorityProtocol;

/// One dispatch's identity, as the resize stack needs to see it.
///
/// All three permit fields travel together because every durable write in
/// `build_pod_permits` is fenced by all three: a stale actor holding an old
/// `fencing_token` cannot advance a lifecycle it no longer owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResizeAdmissionRequest {
    /// The task run being dispatched (`build_pod_permits` PRIMARY KEY).
    pub task_run_id: String,
    /// Immutable permit identity, from `BuildPodPermitRepository::acquire`.
    pub permit_id: String,
    /// Monotonic ownership fence, from the same acquire.
    pub fencing_token: i64,
    /// The protocol the Job was **rendered with**, carried out of
    /// [`djinn_runtime::RunHandle::launcher_authority_protocol`]. This is the
    /// server's decision, and it is the `effective` side of the
    /// observed-vs-effective agreement check against the stored Pod.
    pub effective_protocol: LauncherAuthorityProtocol,
}

/// A dispatch the resize stack has admitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResizeAdmissionOutcome {
    /// `resize-v2`: the admitted ceiling is durable and the birth downsize is
    /// confirmed from `status.initContainerStatuses`.
    BirthConfirmed {
        /// The Pod this run is fenced to — observed from a fresh Pod GET, never
        /// inferred from the Job.
        pod_uid: String,
        /// The write-once ceiling, read back from the durable permit row.
        admitted_cpu_millicores: i64,
    },
    /// `leaf-v1`: the launcher owns each invocation leaf's `cpu.max`. Nothing
    /// was captured and no `pods/resize` PATCH was issued.
    LeafAuthority,
}

/// A dispatch the resize stack refused. **The worker session must not start.**
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct ResizeAdmissionRefused {
    /// Why, rendered for the dispatch error and the operator log.
    pub reason: String,
    /// Whether a UID-fenced delete was issued for the observed Pod. A refusal
    /// that leaves the Pod alive is still a refusal; this only reports what
    /// happened to it.
    pub pod_deleted: bool,
}

/// The server-side resize stack, as the dispatch seam consumes it.
#[async_trait]
pub trait TaskRunResizeAdmission: Send + Sync {
    /// Drive the post-admission bootstrap to a decision for one dispatch.
    ///
    /// Blocks until the launcher sidecar is admitted and its birth downsize is
    /// confirmed, until the implementation's wait budget is exhausted, or until
    /// the bootstrap refuses permanently.
    ///
    /// # Errors
    ///
    /// [`ResizeAdmissionRefused`] when the run may not dispatch.
    async fn admit_dispatch(
        &self,
        request: &ResizeAdmissionRequest,
    ) -> Result<ResizeAdmissionOutcome, ResizeAdmissionRefused>;

    /// Called at the moment a worker session actually starts.
    ///
    /// This is the gate's *absence* detector, not an assertion: with
    /// [`Self::admit_dispatch`] standing in front of the dispatch site the
    /// implementation's unadmitted counter is structurally unreachable and stays
    /// zero. Delete the gate — or turn its refusal into a log-and-continue — and
    /// it becomes non-zero on the first early dispatch, whether or not anyone
    /// remembered to write an assertion about ordering.
    fn record_dispatch_started(&self, task_run_id: &str);
}
