//! Per-run handle returned by [`crate::SessionRuntime::prepare`].
//!
//! Phase 2 PR 1 — fields mirror the shape the `LocalDockerRuntime` will need
//! in PR 6 (container id, IPC socket path), plus a `pod_ref` placeholder for
//! the eventual Kubernetes backend.  No behaviour is attached to this type
//! yet — it's a dumb record that the `SessionRuntime` impls construct in
//! `prepare` and consume in `attach_stdio` / `cancel` / `teardown`.

use std::path::PathBuf;
use std::time::SystemTime;

/// Opaque handle identifying one in-flight task-run inside a
/// [`crate::SessionRuntime`].
#[derive(Debug, Clone)]
pub struct RunHandle {
    /// Globally unique task-run id (uuid v7, lowercase hex).
    pub task_run_id: String,
    /// Docker container id when the runtime is [`LocalDockerRuntime`]; `None`
    /// for backends that don't use Docker (e.g. `TestRuntime`).
    pub container_id: Option<String>,
    /// Kubernetes `namespace/pod` reference for the eventual remote backend;
    /// reserved, always `None` today.
    pub pod_ref: Option<String>,
    /// Host-side path of the Unix domain socket the in-container worker dials
    /// to reach `SupervisorServices`.  For runtimes that speak in-process
    /// (e.g. `TestRuntime`) this is a dummy path that is never opened.
    pub ipc_socket: PathBuf,
    /// Wall-clock time `prepare` returned — used for debug tracing and for
    /// computing overall task-run latency in the coordinator.
    pub started_at: SystemTime,
}
