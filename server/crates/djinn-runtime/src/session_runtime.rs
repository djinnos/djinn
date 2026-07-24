//! [`SessionRuntime`] — the object-safe async trait that hides "how a
//! task-run actually executes" from the coordinator.
//!
//! Phase 2 PR 1 — trait definition plus a [`RuntimeError`] enum.  Impls
//! arrive in later PRs:
//!
//! - [`crate::TestRuntime`] (PR 1, stub in this PR) — in-process, for tests.
//! - `LocalDockerRuntime` (PR 6) — spawns a Docker container per run.
//! - `RemoteKubernetesRuntime` (Phase 3+) — dispatches to a pod via the
//!   cluster API.
//!
//! The trait is deliberately narrow: four verbs (`prepare`, `attach_stdio`,
//! `cancel`, `teardown`) bounded by `async_trait` to keep it object-safe.
//! Any richer contract (progress callbacks, tracing hooks) sits on top of
//! these via the [`crate::BiStream`] the runtime hands back.

use async_trait::async_trait;
use thiserror::Error;

use crate::credentials::ResolvedCredentials;
use crate::handle::RunHandle;
use crate::spec::{TaskRunReport, TaskRunSpec};
use crate::stream::BiStream;

/// Failure modes the runtime surface can return.
///
/// Variants are intentionally coarse — callers route on the category, not on
/// the specific cause (that travels in the wrapped message).  Additional
/// variants will be added as the backends land (`Docker(bollard::Error)`,
/// `Kubernetes(...)`).
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// `prepare` could not materialise the run environment (container
    /// failed to start, workspace clone failed, socket bind failed, …).
    #[error("prepare failed: {0}")]
    Prepare(String),
    /// `attach_stdio` could not wire the duplex stream — usually an IPC
    /// handshake timeout or a container that died before accept.
    #[error("attach_stdio failed: {0}")]
    Attach(String),
    /// The worker never completed its startup handshake within the deadline —
    /// the Pod failed to start (image pull, unschedulable, crash-loop). Distinct
    /// from `Attach` so the dispatch layer can treat it as an infra stall
    /// (teardown + breaker failover) rather than a generic attach failure. The
    /// string is the task_run_id. Surfaced by the Kubernetes `attach_stdio`.
    #[error("worker handshake timed out: {0}")]
    HandshakeTimeout(String),
    /// `cancel` could not deliver the termination signal to the run.
    #[error("cancel failed: {0}")]
    Cancel(String),
    /// `teardown` failed to collect the terminal report or clean the
    /// per-run resources (container removal, socket unlink, tempdir drop).
    #[error("teardown failed: {0}")]
    Teardown(String),
    /// Catch-all for internal invariant violations that are not one of the
    /// lifecycle-stage failures above.
    #[error("runtime internal: {0}")]
    Internal(String),
    /// The project targeted by `prepare` has no per-project devcontainer
    /// image ready yet — the Kubernetes backend cannot dispatch a task
    /// until the image controller has built and pushed one. The string
    /// is the project id so surrounding logs can correlate.
    ///
    /// Surfaced by [`SessionRuntime::prepare`] on the Kubernetes path.
    /// The UI devcontainer banner (Phase 3 PR 6) is the user-facing
    /// recovery path; the runtime just fails fast so the slot actor
    /// doesn't leak a half-prepared Job.
    #[error("devcontainer missing for project {0}")]
    DevcontainerMissing(String),
}

/// Best-effort result of capturing a worker Pod's last log lines after an
/// infra-death.  Carries the bounded log tail plus structured fetch metadata
/// so callers can persist both on the matching `task_attempt`.
#[derive(Clone, Debug)]
pub struct InfraDeathLogTailCapture {
    /// The captured log tail, already truncated to the DB bound.
    /// `None` when capture failed or the Pod had no logs.
    pub log_tail: Option<String>,
    /// Version of the self-describing attempt-evidence payload.
    pub schema_version: u8,
    /// Pod identity and the container selected before logs were requested.
    pub pod_name: Option<String>,
    pub pod_uid: Option<String>,
    pub container_name: Option<String>,
    /// Terminal status observed before the log request.
    pub container_exit_reason: Option<String>,
    pub container_exit_code: Option<i32>,
    /// Byte accounting for the v2 head/tail frame.
    pub head_bytes: usize,
    pub tail_bytes: usize,
    pub omitted_bytes: usize,
    /// Ordered names of transformations applied before framing.
    pub sanitizers: Vec<String>,
    /// Machine-readable error class when capture failed
    /// (e.g. `"pod_not_found"`, `"timeout"`, `"empty_logs"`).
    /// `None` when capture succeeded.
    pub fetch_error_class: Option<String>,
    /// Human-readable detail for logging / debugging.
    pub fetch_error_detail: Option<String>,
}

/// Object-safe lifecycle interface every runtime backend implements.
///
/// Implementations own any per-run state (container ids, socket paths,
/// tempdirs) behind [`RunHandle`].  The coordinator never inspects that
/// state — it just threads the handle back into the next method.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Materialise the run environment — clone the workspace, start the
    /// container, open the IPC socket — and return a handle the caller
    /// threads into the remaining methods.
    ///
    /// `credentials` carries the per-role LLM provider credentials the host
    /// resolved at dispatch time (Phase 7a). Kubernetes-backed runtimes
    /// project these into the worker Pod via a Secret-mount; in-process
    /// test runtimes are free to ignore them.
    async fn prepare(
        &self,
        spec: &TaskRunSpec,
        credentials: &ResolvedCredentials,
    ) -> Result<RunHandle, RuntimeError>;

    /// Attach to the duplex stream created by `prepare`.  Called exactly
    /// once per handle after `prepare` returns.
    async fn attach_stdio(&self, handle: &RunHandle) -> Result<BiStream, RuntimeError>;

    /// Request graceful cancellation.  Implementations should deliver SIGTERM
    /// (or the backend-equivalent), wait a bounded grace period, then escalate
    /// to SIGKILL.  Returns once the cancellation signal has been *delivered*
    /// — waiting for the process to exit is `teardown`'s job.
    async fn cancel(&self, handle: &RunHandle) -> Result<(), RuntimeError>;

    /// Collect the terminal [`TaskRunReport`] and clean up per-run resources
    /// (container removal, socket unlink, tempdir drop).  Consumes the
    /// handle so no further calls can be made against it.
    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError>;

    /// Wait until the run's backing infrastructure has *terminally died*
    /// (the worker process is gone and cannot reconnect), returning a short
    /// human-readable reason string (e.g. `"OOMKilled (exit 137)"`,
    /// `"BackoffLimitExceeded"`).
    ///
    /// This is the host-side liveness watch the dispatch runner races against
    /// the worker's terminal-report stream: when the worker is SIGKILLed (OOM,
    /// node eviction) the RPC connection can linger half-open, so the report
    /// stream never closes and the host would otherwise stay blind until the
    /// generic 30-minute idle stall reaper collected it — mis-attributing an
    /// OOM to a "stall". A runtime that can observe its backing Job/Pod
    /// status resolves this future the moment that infra is terminally dead,
    /// letting the runner finalize the run with the real reason and free the
    /// slot promptly.
    ///
    /// The default implementation never resolves (pends forever): in-process
    /// runtimes have no separable infra that can die out from under the
    /// stream, so racing against this future is a no-op for them. Only the
    /// Kubernetes backend overrides it.
    ///
    /// Implementations MUST only resolve on a *terminal* condition — never on
    /// a transient connection blip or a Pod that might still be scheduling /
    /// restarting — so a legitimate in-flight run is never declared dead.
    async fn watch_infra_death(&self, _handle: &RunHandle) -> String {
        std::future::pending().await
    }

    /// Best-effort capture of the worker Pod's last log lines after an
    /// infra-death has been detected.  Called once between
    /// `watch_infra_death` resolving and `teardown` deleting the Job, so
    /// the Pod may still exist on the apiserver.
    ///
    /// The default implementation returns `None` (no capture).  Only the
    /// Kubernetes backend overrides this.
    ///
    /// Implementations MUST:
    /// - Use a short timeout (≤ 10 s) so the capture never blocks teardown.
    /// - Frame the captured tail to the DB bound (8000 bytes).
    /// - Return `None` on any failure — log-tail capture is best-effort
    ///   diagnostic enrichment and must never prevent teardown or task
    ///   finalization.
    async fn capture_infra_death_log_tail(
        &self,
        _handle: &RunHandle,
    ) -> Option<InfraDeathLogTailCapture> {
        None
    }
}

/// Compile-time assertion that [`SessionRuntime`] is object-safe — any
/// change to the trait that breaks `dyn SessionRuntime` will fail this
/// function's type check.
#[allow(dead_code)]
fn _obj_safe(_: &dyn SessionRuntime) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_runtime_is_object_safe() {
        // Compile-only: if this file compiles, `dyn SessionRuntime` is
        // valid, which is all we need to guarantee for PR 1.
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn SessionRuntime>();
    }
}
