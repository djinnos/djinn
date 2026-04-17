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
}
