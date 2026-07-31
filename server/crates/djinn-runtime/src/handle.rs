//! Per-run handle returned by [`crate::SessionRuntime::prepare`].
//!
//! A dumb record that the `SessionRuntime` impls construct in `prepare` and
//! consume in `attach_stdio` / `cancel` / `teardown`. The worker reaches
//! `SupervisorServices` through the djinn-server-wide TCP listener bound at
//! boot (`serve_on_tcp` on `$DJINN_RPC_ADDR`), not through a per-run socket —
//! so this struct does not carry a transport endpoint.

use std::time::SystemTime;

/// Opaque handle identifying one in-flight task-run inside a
/// [`crate::SessionRuntime`].
#[derive(Debug, Clone)]
pub struct RunHandle {
    /// Globally unique task-run id (uuid v7, lowercase hex).
    pub task_run_id: String,
    /// Container id for runtimes that spawn one (unused today; reserved for
    /// future in-process Docker fallback if it ever returns).
    pub container_id: Option<String>,
    /// Kubernetes `namespace/pod` reference when the runtime is
    /// [`crate::local_docker`-replaced `KubernetesRuntime`]; `None` for
    /// `TestRuntime`.
    pub pod_ref: Option<String>,
    /// Wall-clock time `prepare` returned — used for debug tracing and for
    /// computing overall task-run latency in the coordinator.
    pub started_at: SystemTime,
    /// `metadata.uid` of the **Job** the runtime just created, when it created
    /// one.
    ///
    /// This is deliberately named `job_uid` and not `pod_uid`: `prepare` returns
    /// as soon as the Job POST is confirmed and never waits for — or sees — a
    /// Pod. The permit relation binds this value through
    /// `BuildPodPermitRepository::bind_or_refresh_job_uid`; the Pod UID is a
    /// separate fence obtained later from a fresh Pod GET. Conflating the two
    /// would fence every later resize and delete against an object that is not
    /// the one being resized.
    ///
    /// `None` for runtimes that create no Job (`TestRuntime`).
    pub job_uid: Option<String>,
    /// The launcher authority protocol this run's Job was **rendered with**.
    ///
    /// The render is the server's decision: it comes from the resolved dispatch
    /// image's migration-166 `authority_protocol` and is applied to the Job
    /// before the POST. Carrying it out of `prepare` is what lets the dispatch
    /// seam branch on `leaf-v1` vs `resize-v2` without re-deriving the decision
    /// (and drifting from it), and it is the `effective` side of the
    /// observed-vs-effective protocol agreement check the resize bootstrap
    /// performs against the stored Pod.
    ///
    /// `None` for runtimes that render no launcher sidecar (`TestRuntime`).
    pub launcher_authority_protocol: Option<djinn_launcher_protocol::LauncherAuthorityProtocol>,
}
