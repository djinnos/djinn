//! `LocalDockerRuntime` — [`crate::SessionRuntime`] impl that runs each
//! task-run inside a fresh `djinn-agent-runtime` Docker container.
//!
//! Phase 2 PR 6 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! ## Lifecycle
//!
//! 1. `prepare(spec)`
//!    - Host-side ephemeral clone via [`MirrorManager::clone_ephemeral`].  The
//!      returned [`Workspace`] owns the tempdir that backs `/workspace` —
//!      stored inside `RunState` to keep the bind mount alive for the full
//!      run.
//!    - Spawn a one-shot `SupervisorServices` RPC server on
//!      `<ipc_root>/<task_run_id>.sock` via the injected [`IpcServerFactory`].
//!      (The factory trait hides the `djinn-supervisor` dep — see the
//!      "Cycle avoidance" note below.)
//!    - `docker create` the container with the six bind mounts, env block,
//!      and HostConfig defaults from [`bollard_ops`].
//!    - `docker start`, attach stdin, write the bincode-framed
//!      [`TaskRunSpec`] to stdin, shut the write half.
//!    - Return a [`RunHandle`] carrying the container id + host socket path.
//! 2. `attach_stdio(&handle)` — PR 6 returns an empty [`BiStream`].  The
//!    launcher-side stream bridge (tailing container stdout →
//!    `WorkerEvent`) lands in a later PR along with the worker's streaming
//!    emitter.
//! 3. `cancel(&handle, grace)` — send [`ControlMsg::Cancel`] on a
//!    best-effort dial to the socket, then `docker kill --signal=SIGTERM`.
//!    Idempotent — a stopped container yields `Ok(())`.
//! 4. `teardown(handle)` — `docker remove --force`, drop the workspace
//!    tempdir, cancel + join the `ServeHandle`, unlink the socket.
//!
//! ## Cycle avoidance (`djinn-supervisor` dep)
//!
//! `djinn-supervisor` already depends on `djinn-runtime` (it re-uses
//! [`crate::wire`] + [`crate::TaskRunSpec`]).  Taking a hard dep the other
//! direction would cycle.  Instead this module defines the [`IpcServerFactory`]
//! trait — `LocalDockerRuntime` accepts an `Arc<dyn IpcServerFactory>` at
//! construction and calls it to bind the per-run socket.  PR 7's
//! supervisor-runner wires a real factory that internally calls
//! `djinn_supervisor::services::server::serve_on_unix_socket` with a
//! `DirectServices`.

pub mod bollard_ops;
pub mod config;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bollard::Docker;
use djinn_workspace::Workspace;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tracing::{debug, error, warn};

use crate::handle::RunHandle;
use crate::session_runtime::{RuntimeError, SessionRuntime};
use crate::spec::{TaskRunOutcome, TaskRunReport, TaskRunSpec};
use crate::stream::BiStream;
use crate::wire::{ControlMsg, write_frame};

pub use config::{LocalDockerConfig, LocalDockerConfigBuilder};

/// Opaque handle returned by [`IpcServerFactory::bind`].
///
/// The runtime keeps this alive for the duration of the task-run; dropping
/// or calling [`IpcServerHandle::shutdown`] at teardown tears the server
/// down.  Using `Send + 'static` (rather than `Sync`) keeps the trait
/// flexible for the `ServeHandle` carry.
#[async_trait]
pub trait IpcServerHandle: Send + 'static {
    /// Cancel the server's accept loop + any in-flight connection, then
    /// await the background tasks.  Called exactly once, from `teardown`.
    async fn shutdown(self: Box<Self>);
}

/// Host-side factory that binds a [`SupervisorServices`]-aware unix-socket
/// RPC server for one task-run.  Hides `djinn-supervisor` from
/// `djinn-runtime` to keep the dep graph acyclic.
#[async_trait]
pub trait IpcServerFactory: Send + Sync + 'static {
    /// Bind the server at `socket_path` and return a handle that keeps the
    /// server alive until [`IpcServerHandle::shutdown`] is awaited.
    async fn bind(&self, socket_path: PathBuf) -> std::io::Result<Box<dyn IpcServerHandle>>;
}

/// In-flight task-run state owned by [`LocalDockerRuntime`].
///
/// Hangs on to everything whose drop would kill the container environment
/// (tempdir, server handle) so the coordinator can drive `attach_stdio` →
/// `cancel` → `teardown` without any of the per-run resources evaporating.
struct RunState {
    socket_path: PathBuf,
    /// Kept alive so `/workspace` bind stays valid — the `Drop` impl on the
    /// inner `TempDir` is what rms the host-side clone after teardown.
    _workspace: Workspace,
    /// Shutdown handle for the SupervisorServices RPC server; `Option` so
    /// `teardown` can `take()` it out for `.shutdown().await`.
    server_handle: Option<Box<dyn IpcServerHandle>>,
}

/// Docker-backed [`SessionRuntime`].
///
/// Constructed once at launcher startup.  Clone the inner `Arc` freely —
/// per-run state is stored in the internal map keyed by `task_run_id`.
pub struct LocalDockerRuntime {
    docker: Docker,
    config: LocalDockerConfig,
    mirror: Arc<dyn MirrorBackend>,
    ipc_factory: Arc<dyn IpcServerFactory>,
    runs: Arc<Mutex<HashMap<String, RunState>>>,
}

impl LocalDockerRuntime {
    /// Build a runtime with explicit deps.
    pub fn new(
        docker: Docker,
        config: LocalDockerConfig,
        mirror: Arc<dyn MirrorBackend>,
        ipc_factory: Arc<dyn IpcServerFactory>,
    ) -> Self {
        Self {
            docker,
            config,
            mirror,
            ipc_factory,
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Convenience: connect to the local docker daemon via the default unix
    /// socket (`/var/run/docker.sock`) and forward everything else to
    /// [`Self::new`].
    pub fn connect_local(
        config: LocalDockerConfig,
        mirror: Arc<dyn MirrorBackend>,
        ipc_factory: Arc<dyn IpcServerFactory>,
    ) -> Result<Self, RuntimeError> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| RuntimeError::Internal(format!("docker connect: {e}")))?;
        Ok(Self::new(docker, config, mirror, ipc_factory))
    }
}

/// Minimal mirror surface the runtime needs — the object-safe subset of
/// [`djinn_workspace::MirrorManager`].  Implementations typically wrap a
/// shared `Arc<MirrorManager>` and forward.
///
/// Having the abstraction here keeps `djinn-runtime` usable from
/// integration tests that want a synthetic workspace without running a
/// full mirror clone pipeline.
#[async_trait]
pub trait MirrorBackend: Send + Sync + 'static {
    async fn clone_ephemeral(
        &self,
        project_id: &str,
        branch: &str,
    ) -> Result<Workspace, RuntimeError>;
}

/// Blanket-style impl for `Arc<MirrorManager>` so production code can pass
/// the manager directly without writing a thin wrapper.
#[async_trait]
impl MirrorBackend for djinn_workspace::MirrorManager {
    async fn clone_ephemeral(
        &self,
        project_id: &str,
        branch: &str,
    ) -> Result<Workspace, RuntimeError> {
        djinn_workspace::MirrorManager::clone_ephemeral(self, project_id, branch)
            .await
            .map_err(|e| RuntimeError::Prepare(format!("mirror clone_ephemeral: {e}")))
    }
}

#[async_trait]
impl SessionRuntime for LocalDockerRuntime {
    async fn prepare(&self, spec: &TaskRunSpec) -> Result<RunHandle, RuntimeError> {
        // 1. Host-side ephemeral clone.
        let workspace = self
            .mirror
            .clone_ephemeral(&spec.project_id, &spec.base_branch)
            .await?;
        let workspace_path = workspace.path_buf();

        // 2. Allocate a task-run id and bind the RPC socket.
        let task_run_id = uuid::Uuid::now_v7().as_simple().to_string();
        let socket_path = self.config.socket_path_for(&task_run_id);
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                RuntimeError::Prepare(format!(
                    "create ipc parent {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let server_handle = self
            .ipc_factory
            .bind(socket_path.clone())
            .await
            .map_err(|e| RuntimeError::Prepare(format!("bind ipc socket: {e}")))?;

        // 3. docker create.
        let container_socket = format!("/var/run/djinn/{task_run_id}.sock");
        let binds = bollard_ops::default_binds(&self.config, &workspace_path);
        let host_config =
            bollard_ops::default_host_config(&self.config, bollard_ops::binds_as_strings(&binds));
        let env = bollard_ops::default_env(&container_socket);
        let container_name = format!("djinn-task-{task_run_id}");
        let cmd = vec![
            "/usr/local/bin/djinn-agent-worker".to_string(),
            "--ipc-socket".to_string(),
            container_socket.clone(),
        ];

        let create_resp = bollard_ops::create_container_for_run(
            &self.docker,
            bollard_ops::CreateArgs {
                image: &self.config.image_tag,
                name: &container_name,
                cmd,
                env,
                host_config,
                workdir: "/workspace",
            },
        )
        .await
        .map_err(|e| RuntimeError::Prepare(format!("docker create: {e}")))?;
        let container_id = create_resp.id;

        // 4. docker start then attach + pipe spec to stdin.
        if let Err(e) = bollard_ops::start_container(&self.docker, &container_id).await {
            // Best-effort cleanup so we don't leak a created-but-not-started
            // container.
            let _ = bollard_ops::remove_container(&self.docker, &container_id).await;
            return Err(RuntimeError::Prepare(format!("docker start: {e}")));
        }
        let mut attach = bollard_ops::attach_stdin_stdout(&self.docker, &container_id)
            .await
            .map_err(|e| RuntimeError::Prepare(format!("docker attach: {e}")))?;
        if let Err(e) = bollard_ops::pipe_spec_to_stdin(&mut attach, spec).await {
            let _ = bollard_ops::remove_container(&self.docker, &container_id).await;
            return Err(e);
        }
        // Dropping `attach.output` stream is fine here — PR 6 does not tail
        // it; PR 7+ will hold it alive in `attach_stdio`.
        drop(attach);

        // 5. Record run state so the remaining lifecycle methods can find it.
        {
            let mut runs = self.runs.lock().await;
            runs.insert(
                task_run_id.clone(),
                RunState {
                    socket_path: socket_path.clone(),
                    _workspace: workspace,
                    server_handle: Some(server_handle),
                },
            );
        }

        Ok(RunHandle {
            task_run_id,
            container_id: Some(container_id),
            pod_ref: None,
            ipc_socket: socket_path,
            started_at: SystemTime::now(),
        })
    }

    async fn attach_stdio(&self, _handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        // PR 6 returns a bare in-memory pair.  The real stdout → WorkerEvent
        // bridge (length-prefixed frames coming off `docker attach`) lands
        // alongside the worker's streaming emitter in a later PR —
        // flagging this here so nobody wires a dispatch path to it.
        let (stream, _events_tx, _requests_rx) = BiStream::new_in_memory(16);
        Ok(stream)
    }

    async fn cancel(&self, handle: &RunHandle) -> Result<(), RuntimeError> {
        // Best-effort: send a ControlMsg::Cancel on a fresh dial to the IPC
        // socket.  If the worker has not yet connected, the second attempt
        // (docker kill) handles the termination path.
        if let Err(e) = send_cancel_frame(&handle.ipc_socket).await {
            debug!(error = %e, "cancel: ipc Cancel frame best-effort failed");
        }

        if let Some(container_id) = handle.container_id.as_deref() {
            bollard_ops::kill_container(&self.docker, container_id, "SIGTERM")
                .await
                .map_err(|e| RuntimeError::Cancel(format!("docker kill: {e}")))?;
        }
        Ok(())
    }

    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        let state = {
            let mut runs = self.runs.lock().await;
            runs.remove(&handle.task_run_id)
        };

        if let Some(container_id) = handle.container_id.as_deref() {
            if let Err(e) = bollard_ops::remove_container(&self.docker, container_id).await {
                warn!(error = %e, container_id, "teardown: remove_container failed");
            }
        }

        if let Some(mut state) = state {
            if let Some(srv) = state.server_handle.take() {
                srv.shutdown().await;
            }
            if let Err(e) = tokio::fs::remove_file(&state.socket_path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    debug!(error = %e, path = %state.socket_path.display(), "teardown: unlink socket");
                }
            }
            // `state._workspace` drops here -> TempDir drops -> host clone
            // deleted.
        } else {
            warn!(task_run_id = %handle.task_run_id, "teardown: no RunState found");
        }

        // PR 6 does not yet collect a terminal TaskRunReport over the wire —
        // that arrives on the stdout-tailing BiStream in a later PR.
        // For now synthesise an Interrupted report so callers observe a
        // completed lifecycle.  PR 7 replaces this with the real report.
        Ok(TaskRunReport {
            task_run_id: handle.task_run_id,
            outcome: TaskRunOutcome::Interrupted,
            stages_completed: Vec::new(),
        })
    }
}

/// Best-effort `ControlMsg::Cancel` delivery via a fresh Unix socket dial.
///
/// The `serve_on_unix_socket` server is 1:1 with the worker — if the worker
/// already claimed the accept slot this call fails, which is fine: `cancel`
/// falls through to `docker kill` anyway.
async fn send_cancel_frame(path: &std::path::Path) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(path).await?;
    let frame = ControlMsg::Cancel;
    if let Err(e) = write_frame(&mut stream, &frame).await {
        error!(error = %e, "send_cancel_frame: write failed");
        return Err(std::io::Error::other(e));
    }
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Lifecycle-flow smoke tests that exercise the module boundary without
    //! requiring a real docker daemon.  Docker-dependent coverage lives in
    //! `tests/local_docker_smoke.rs` under `#[ignore]` + `DJINN_TEST_DOCKER=1`.

    use super::*;

    #[test]
    fn config_default_roundtrips_through_builder() {
        let built = LocalDockerConfig::builder().build();
        let default = LocalDockerConfig::default();
        assert_eq!(built.image_tag, default.image_tag);
        assert_eq!(built.memory_limit_bytes, default.memory_limit_bytes);
    }

    #[test]
    fn ipc_server_factory_is_object_safe() {
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn IpcServerFactory>();
        assert_object_safe::<dyn IpcServerHandle>();
        assert_object_safe::<dyn MirrorBackend>();
    }
}
