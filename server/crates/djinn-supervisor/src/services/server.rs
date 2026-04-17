//! Launcher-side bincode-over-unix-socket server for [`SupervisorServices`].
//!
//! Phase 2 PR 5 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! The `LocalDockerRuntime` (PR 6) will call [`serve_on_unix_socket`] just
//! before launching the container: it binds a per-task-run socket at
//! `<ipc_root>/<task_run_id>.sock`, accepts exactly one connection (the
//! worker's `RpcServices::connect`), and routes every inbound
//! [`ServiceRpcRequest`] to a host-side `Arc<dyn SupervisorServices>` impl
//! (`DirectServices` today, maybe a fancier composite later).
//!
//! ## Scope of this PR
//!
//! The server accepts **one** connection per socket lifetime — the worker
//! container is 1:1 with the socket, so connection pooling is not a concern.
//! Once the worker disconnects the reader task returns, the writer task
//! drains, and the `ServeHandle` resolves.  Callers that want to tear the
//! server down early can call [`ServeHandle::cancel`] or drop the handle.
//!
//! ## Cancellation
//!
//! [`serve_on_unix_socket`] returns a [`ServeHandle`] carrying a
//! [`CancellationToken`].  Firing that token causes both the accept loop
//! (before a connection arrives) and the reader/writer pair (after) to tear
//! down.  When the launcher observes the worker's terminal report and wants
//! to reclaim the socket, it cancels the handle and awaits the join handle.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use djinn_runtime::wire::{ControlMsg, read_frame, write_frame};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::SupervisorServices;
use super::wire::{Frame, FramePayload, ServiceRpcRequest, ServiceRpcResponse};

/// Handle returned by [`serve_on_unix_socket`].
///
/// Dropping the handle does not automatically cancel the server — the
/// accept loop keeps the socket open until the cancellation token fires or
/// the worker disconnects.  Call [`ServeHandle::cancel`] to force-stop.
pub struct ServeHandle {
    /// Fire to tear the accept loop and any active connection down.
    pub cancel_server: CancellationToken,
    /// Await to join the accept loop (and the per-connection tasks it spawned).
    pub join: JoinHandle<()>,
    /// Path the socket was bound to — exposed for debug / teardown unlink.
    pub socket_path: PathBuf,
}

impl ServeHandle {
    /// Signal the server to stop.
    pub fn cancel(&self) {
        self.cancel_server.cancel();
    }
}

/// Bind a Unix-domain socket at `path` and spawn the accept loop.
///
/// `services` is used to dispatch every incoming RPC.  Typically this is a
/// `DirectServices` the launcher owns, but any `Arc<dyn SupervisorServices>`
/// works (tests inject a minimal fake).
///
/// Returns once the socket is bound and the accept loop is running — the
/// worker can connect immediately.
pub async fn serve_on_unix_socket<P: AsRef<Path>>(
    path: P,
    services: Arc<dyn SupervisorServices>,
) -> io::Result<ServeHandle> {
    let socket_path = path.as_ref().to_path_buf();
    // Tolerate a stale socket from a previous run.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    info!(socket = %socket_path.display(), "SupervisorServices RPC server listening");

    let cancel_server = CancellationToken::new();
    let cancel_child = cancel_server.clone();
    let services_arc = services.clone();
    let path_for_task = socket_path.clone();

    let join = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = cancel_child.cancelled() => {
                debug!(socket = %path_for_task.display(), "server cancelled before connection");
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        debug!(socket = %path_for_task.display(), "worker connected");
                        run_connection(stream, services_arc, cancel_child.clone()).await;
                    }
                    Err(e) => {
                        error!(error = %e, "SupervisorServices accept failed");
                    }
                }
            }
        }
        // Best-effort unlink on the way out.
        let _ = std::fs::remove_file(&path_for_task);
    });

    Ok(ServeHandle {
        cancel_server,
        join,
        socket_path,
    })
}

/// Drive the read/write loop for a single connection.
async fn run_connection(
    stream: tokio::net::UnixStream,
    services: Arc<dyn SupervisorServices>,
    cancel: CancellationToken,
) {
    let (read_half, write_half) = stream.into_split();
    let (reply_tx, reply_rx) = mpsc::channel::<Frame>(64);

    let writer = tokio::spawn(writer_loop(write_half, reply_rx, cancel.clone()));
    reader_loop(read_half, services, reply_tx, cancel.clone()).await;

    // Closing `reply_tx` drains the writer naturally.
    let _ = writer.await;
}

async fn reader_loop(
    mut read_half: OwnedReadHalf,
    services: Arc<dyn SupervisorServices>,
    reply_tx: mpsc::Sender<Frame>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("server reader: cancelled");
                return;
            }
            res = read_frame::<_, Frame>(&mut read_half) => {
                match res {
                    Ok(frame) => {
                        let correlation_id = frame.correlation_id;
                        match frame.payload {
                            FramePayload::Rpc(req) => {
                                let services_ref = services.clone();
                                let reply_tx = reply_tx.clone();
                                // Spawn per-request so slow dispatches don't
                                // block subsequent frames on the same socket.
                                tokio::spawn(async move {
                                    let resp = dispatch(services_ref, req).await;
                                    let reply = Frame {
                                        correlation_id,
                                        payload: FramePayload::RpcReply(resp),
                                    };
                                    if reply_tx.send(reply).await.is_err() {
                                        warn!("server reader: writer dropped before reply");
                                    }
                                });
                            }
                            FramePayload::Control(ControlMsg::Cancel) => {
                                debug!("server reader: Cancel from worker");
                                services.cancel().cancel();
                            }
                            FramePayload::Control(ControlMsg::Shutdown) => {
                                debug!("server reader: Shutdown from worker");
                                cancel.cancel();
                                return;
                            }
                            other => {
                                debug!(?other, "server reader: unhandled frame");
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "server reader: stream closed");
                        return;
                    }
                }
            }
        }
    }
}

async fn writer_loop(
    mut write_half: OwnedWriteHalf,
    mut rx: mpsc::Receiver<Frame>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("server writer: cancelled");
                let _ = write_half.shutdown().await;
                return;
            }
            frame = rx.recv() => {
                let Some(frame) = frame else {
                    debug!("server writer: reply channel closed");
                    let _ = write_half.shutdown().await;
                    return;
                };
                if let Err(e) = write_frame(&mut write_half, &frame).await {
                    error!(error = %e, "server writer: failed to write reply");
                    return;
                }
            }
        }
    }
}

/// Match a [`ServiceRpcRequest`] variant onto the corresponding trait method.
async fn dispatch(
    services: Arc<dyn SupervisorServices>,
    req: ServiceRpcRequest,
) -> ServiceRpcResponse {
    match req {
        ServiceRpcRequest::LoadTask { task_id } => {
            let result = services.load_task(task_id).await;
            ServiceRpcResponse::LoadTask(result)
        }
        ServiceRpcRequest::ExecuteStage {
            task,
            workspace,
            role_kind,
            task_run_id,
            spec,
        } => {
            // Rehydrate the `Workspace` wrapper from the serializable ref.
            // `attach_existing` is a no-op-drop variant — the runtime still
            // owns the tempdir that backs the bind mount.
            let workspace = match djinn_workspace::Workspace::attach_existing(
                workspace.path.as_path(),
                workspace.branch.clone(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    return ServiceRpcResponse::ExecuteStage(Err(
                        crate::StageError::Setup(format!("attach workspace: {e}")),
                    ));
                }
            };
            let result = services
                .execute_stage(&task, &workspace, role_kind, &task_run_id, &spec)
                .await;
            ServiceRpcResponse::ExecuteStage(result)
        }
        ServiceRpcRequest::OpenPr { spec, task } => {
            let outcome = services.open_pr(&spec, &task).await;
            ServiceRpcResponse::OpenPr(outcome)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use djinn_core::models::Task;
    use djinn_workspace::Workspace;

    use crate::{RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec};

    /// Minimal fake that returns a canned task on `load_task` and panics on
    /// the other trait methods (the launcher-side tests only exercise the
    /// load_task path).
    struct FakeServices {
        cancel: CancellationToken,
        canned_task_id: String,
    }

    #[async_trait]
    impl SupervisorServices for FakeServices {
        fn cancel(&self) -> &CancellationToken {
            &self.cancel
        }

        async fn load_task(&self, task_id: String) -> Result<Task, String> {
            let mut t = fixture_task();
            t.id = task_id.clone();
            t.title = format!("loaded:{task_id}");
            assert_eq!(task_id, self.canned_task_id);
            Ok(t)
        }

        async fn execute_stage(
            &self,
            _task: &Task,
            _workspace: &Workspace,
            _role_kind: RoleKind,
            _task_run_id: &str,
            _spec: &TaskRunSpec,
        ) -> Result<StageOutcome, StageError> {
            unimplemented!("not exercised in PR 5 server tests")
        }

        async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
            unimplemented!("not exercised in PR 5 server tests")
        }
    }

    fn fixture_task() -> Task {
        Task {
            id: String::new(),
            project_id: "p".into(),
            short_id: "T-1".into(),
            epic_id: None,
            title: String::new(),
            description: "d".into(),
            design: "".into(),
            issue_type: "task".into(),
            status: "open".into(),
            priority: 0,
            owner: "fernando".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            unresolved_blocker_count: 0,
        }
    }

    #[tokio::test]
    async fn server_routes_load_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("svc.sock");

        let services: Arc<dyn SupervisorServices> = Arc::new(FakeServices {
            cancel: CancellationToken::new(),
            canned_task_id: "wire-task-1".into(),
        });
        let handle = serve_on_unix_socket(&sock, services)
            .await
            .expect("bind server");

        // Drive the worker side via RpcServices.
        let client_cancel = CancellationToken::new();
        let (rpc, bg) = super::super::rpc::RpcServices::connect(&sock, client_cancel.clone())
            .await
            .expect("connect rpc");
        let task = rpc
            .load_task("wire-task-1".into())
            .await
            .expect("rpc load_task");
        assert_eq!(task.id, "wire-task-1");
        assert_eq!(task.title, "loaded:wire-task-1");

        // Teardown.
        client_cancel.cancel();
        handle.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = handle.join.await;
    }
}
