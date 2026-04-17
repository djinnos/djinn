//! Worker-side [`SupervisorServices`] impl that speaks bincode over a duplex
//! byte stream.
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`
//! generalises this impl over any `AsyncRead + AsyncWrite + Unpin + Send`
//! transport so the worker can dial either a Unix-domain socket (in-process
//! tests + legacy path) or a TCP connection (the K8s Pod path).
//!
//! The worker process (`djinn-agent-worker`) dials the launcher, performs
//! the transport-specific handshake (none on unix; [`AuthHelloMsg`] on TCP),
//! then hands the resulting read/write halves to [`RpcServices::from_split`].
//! Each trait method then:
//!
//! 1. allocates a fresh `correlation_id` via an atomic counter,
//! 2. parks a `oneshot::Sender` for that id in a shared `pending` map,
//! 3. pushes a [`Frame`] onto the outbound mpsc channel drained by the
//!    writer task,
//! 4. awaits the matching `RpcReply` frame the reader task routed back
//!    through the `oneshot::Receiver`.
//!
//! The writer + reader tasks shut down cleanly when the socket closes or
//! when the supervisor-wide `CancellationToken` fires.
//!
//! # Why the stub stays
//!
//! The supervisor's object-safety assertion ([`_obj_safe`][objsafe]) and the
//! crate-root tests that need a trivial no-op `SupervisorServices` still
//! want a zero-config impl.  [`UnimplementedRpcServices`] fills that role —
//! formerly `StubRpcServices`, re-exported under the old name at the crate
//! root so no downstream call site has to change.
//!
//! [objsafe]: crate::tests::_obj_safe

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use djinn_core::models::Task;
use djinn_runtime::wire::{ControlMsg, WorkspaceRef, read_frame, write_frame};
use djinn_workspace::Workspace;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::SupervisorServices;
use super::wire::{
    AuthHelloMsg, AuthResultMsg, Frame, FramePayload, ServiceRpcRequest, ServiceRpcResponse,
};
use crate::{RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec};

/// Failure mode for [`RpcServices::connect_tcp`].
///
/// Distinguishes three error shapes callers can act on:
///
/// - [`ConnectTcpError::Io`] — the TCP dial, a frame read, or a frame write
///   hit an underlying socket error.  Retry-eligible for transient faults.
/// - [`ConnectTcpError::Rejected`] — the server answered the handshake with
///   an `AuthResult { accepted: false, .. }`.  Carries the server's
///   human-readable reason if any.  Not retry-eligible: the token is bad.
/// - [`ConnectTcpError::Protocol`] — the server's first post-handshake
///   frame was not an `AuthResult` (or was otherwise malformed).  Never
///   expected in production — the server unconditionally replies with an
///   `AuthResult` after the `AuthHello`.
#[derive(Debug, Error)]
pub enum ConnectTcpError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("auth rejected: {0}")]
    Rejected(String),
    #[error("protocol: {0}")]
    Protocol(String),
}

/// Wrap an arbitrary description into an `io::Error` so the `?` operator in
/// [`RpcServices::connect_tcp`] can funnel non-io handshake mishaps through
/// the same `Io` variant without hiding the frame-codec diagnostic.
fn io_other(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg.into())
}

// ── Real RPC client ──────────────────────────────────────────────────────────

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<ServiceRpcResponse>>>>;

/// Bincode RPC client for [`SupervisorServices`].
///
/// The struct itself is transport-agnostic — it holds the outbound mpsc
/// sender, the correlation-id → oneshot map, and the supervisor-wide
/// cancellation token.  Transport-specific setup lives in the
/// [`RpcServices::from_split`] / [`RpcServices::from_stream`] /
/// [`RpcServices::connect_unix`] constructors.
pub struct RpcServices {
    tx: mpsc::Sender<Frame>,
    pending: PendingMap,
    cancel: CancellationToken,
    next_id: AtomicU64,
}

/// Join handle bundle returned by every [`RpcServices`] constructor.
///
/// The caller keeps this around for the lifetime of the task-run and awaits
/// both halves on shutdown so the socket is drained cleanly.
pub struct RpcBackgroundTasks {
    pub reader: JoinHandle<()>,
    pub writer: JoinHandle<()>,
}

impl RpcServices {
    /// Canonical constructor: spin up the reader / writer tasks against a
    /// pre-split byte stream.
    ///
    /// Generic over any `AsyncRead + AsyncWrite + Unpin + Send + 'static`
    /// half-pair so the worker can feed it either a `UnixStream` split or
    /// a `TcpStream` split.  Pre-handshake bytes (e.g. the `AuthHello`
    /// round-trip on TCP) MUST be consumed by the caller before handing
    /// the halves in — this function assumes the stream is positioned at
    /// the start of the post-handshake RPC byte stream.
    pub fn from_split<R, W>(
        read_half: R,
        write_half: W,
        cancel: CancellationToken,
    ) -> (Arc<Self>, RpcBackgroundTasks)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<Frame>(64);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        let services = Arc::new(Self {
            tx,
            pending: pending.clone(),
            cancel: cancel.clone(),
            next_id: AtomicU64::new(1),
        });

        let reader = tokio::spawn(reader_loop(read_half, pending.clone(), cancel.clone()));
        let writer = tokio::spawn(writer_loop(write_half, rx, cancel.clone()));

        (services, RpcBackgroundTasks { reader, writer })
    }

    /// Split a [`UnixStream`] and delegate to [`RpcServices::from_split`].
    pub fn from_unix_stream(
        stream: UnixStream,
        cancel: CancellationToken,
    ) -> (Arc<Self>, RpcBackgroundTasks) {
        let (read_half, write_half) = stream.into_split();
        Self::from_split(read_half, write_half, cancel)
    }

    /// Split a [`TcpStream`] and delegate to [`RpcServices::from_split`].
    pub fn from_stream(
        stream: TcpStream,
        cancel: CancellationToken,
    ) -> (Arc<Self>, RpcBackgroundTasks) {
        let (read_half, write_half) = stream.into_split();
        Self::from_split(read_half, write_half, cancel)
    }

    /// Convenience wrapper: dial `path` via `UnixStream`, then delegate to
    /// [`RpcServices::from_unix_stream`].
    pub async fn connect_unix(
        path: impl AsRef<Path>,
        cancel: CancellationToken,
    ) -> std::io::Result<(Arc<Self>, RpcBackgroundTasks)> {
        let stream = UnixStream::connect(path.as_ref()).await?;
        Ok(Self::from_unix_stream(stream, cancel))
    }

    /// Dial `addr`, perform the [`FramePayload::AuthHello`] handshake, and —
    /// on `AuthResult { accepted: true, .. }` — hand the split stream off to
    /// [`RpcServices::from_split`].
    ///
    /// Called by `djinn-agent-worker` after it has read its projected
    /// ServiceAccount token from `/var/run/secrets/tokens/djinn`.  The
    /// handshake is a single request/response round-trip on `correlation_id
    /// = 0`; after the ack, the same socket enters the normal RPC dispatch
    /// loop unchanged.
    ///
    /// # Errors
    ///
    /// See [`ConnectTcpError`].  Transport/socket errors surface as
    /// [`ConnectTcpError::Io`]; a server-side token rejection surfaces as
    /// [`ConnectTcpError::Rejected`]; anything else the server sends back
    /// in place of an `AuthResult` surfaces as [`ConnectTcpError::Protocol`].
    pub async fn connect_tcp(
        addr: SocketAddr,
        task_run_id: String,
        token: String,
        cancel: CancellationToken,
    ) -> Result<(Arc<Self>, RpcBackgroundTasks), ConnectTcpError> {
        let mut stream = TcpStream::connect(addr).await?;
        info!(%addr, %task_run_id, "tcp dialed launcher; sending AuthHello");

        // 1. Send the AuthHello on correlation_id 0.
        let hello = Frame {
            correlation_id: 0,
            payload: FramePayload::AuthHello(AuthHelloMsg {
                task_run_id: task_run_id.clone(),
                token,
            }),
        };
        write_frame(&mut stream, &hello)
            .await
            .map_err(|e| ConnectTcpError::Io(io_other(format!("write AuthHello: {e}"))))?;

        // 2. Read the AuthResult reply.
        let reply: Frame = read_frame(&mut stream)
            .await
            .map_err(|e| ConnectTcpError::Io(io_other(format!("read AuthResult: {e}"))))?;

        match reply.payload {
            FramePayload::AuthResult(AuthResultMsg {
                accepted: true, ..
            }) => {
                info!(%task_run_id, "tcp auth accepted");
            }
            FramePayload::AuthResult(AuthResultMsg {
                accepted: false,
                error,
            }) => {
                let reason = error.unwrap_or_else(|| "token rejected".into());
                return Err(ConnectTcpError::Rejected(reason));
            }
            other => {
                return Err(ConnectTcpError::Protocol(format!(
                    "expected AuthResult, got {other:?}"
                )));
            }
        }

        // 3. Split the stream and enter the shared dispatch loop.
        let (read_half, write_half) = stream.into_split();
        Ok(Self::from_split(read_half, write_half, cancel))
    }

    /// Allocate a fresh correlation id, send the request, and await the
    /// matching reply.  Returns a transport-level error if the socket closed
    /// before a reply arrived or the response variant did not match the
    /// request shape.
    async fn roundtrip(&self, req: ServiceRpcRequest) -> Result<ServiceRpcResponse, String> {
        let correlation_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<ServiceRpcResponse>();
        self.pending.lock().await.insert(correlation_id, tx);

        let frame = Frame {
            correlation_id,
            payload: FramePayload::Rpc(req),
        };
        if self.tx.send(frame).await.is_err() {
            // Writer task is gone.
            self.pending.lock().await.remove(&correlation_id);
            return Err("rpc writer task dropped".into());
        }

        rx.await.map_err(|_| {
            // Reader task dropped the oneshot without replying — usually
            // because the socket closed before the reply arrived.
            format!("rpc reply channel closed (correlation_id={correlation_id})")
        })
    }
}

#[async_trait]
impl SupervisorServices for RpcServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn load_task(&self, task_id: String) -> Result<Task, String> {
        match self
            .roundtrip(ServiceRpcRequest::LoadTask { task_id })
            .await
        {
            Ok(ServiceRpcResponse::LoadTask(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        // Pack the workspace as a WorkspaceRef so it can cross the wire.
        // `owned_by_runtime` is always `true` on the worker path: the host
        // materialised the bind mount and the worker only attached to it.
        let workspace_ref = WorkspaceRef {
            path: workspace.path().to_path_buf(),
            branch: workspace.branch().to_string(),
            owned_by_runtime: true,
        };
        let req = ServiceRpcRequest::ExecuteStage {
            task: task.clone(),
            workspace: workspace_ref,
            role_kind,
            task_run_id: task_run_id.to_string(),
            spec: spec.clone(),
        };
        match self.roundtrip(req).await {
            Ok(ServiceRpcResponse::ExecuteStage(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => {
                Err(StageError::Setup(format!("rpc transport: {e}")))
            }
            Ok(other) => Err(StageError::Setup(format!(
                "rpc protocol: unexpected reply {other:?}"
            ))),
            Err(e) => Err(StageError::Setup(e)),
        }
    }

    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome {
        let req = ServiceRpcRequest::OpenPr {
            spec: spec.clone(),
            task: task.clone(),
        };
        match self.roundtrip(req).await {
            Ok(ServiceRpcResponse::OpenPr(outcome)) => outcome,
            Ok(ServiceRpcResponse::Err(e)) => TaskRunOutcome::Failed {
                stage: "open_pr".into(),
                reason: format!("rpc transport: {e}"),
            },
            Ok(other) => TaskRunOutcome::Failed {
                stage: "open_pr".into(),
                reason: format!("rpc protocol: unexpected reply {other:?}"),
            },
            Err(e) => TaskRunOutcome::Failed {
                stage: "open_pr".into(),
                reason: e,
            },
        }
    }
}

// ── Reader / writer loops ────────────────────────────────────────────────────

async fn reader_loop<R>(mut read_half: R, pending: PendingMap, cancel: CancellationToken)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("rpc reader: cancelled");
                return;
            }
            res = read_frame::<_, Frame>(&mut read_half) => {
                match res {
                    Ok(frame) => match frame.payload {
                        FramePayload::RpcReply(resp) => {
                            if let Some(tx) = pending.lock().await.remove(&frame.correlation_id) {
                                let _ = tx.send(resp);
                            } else {
                                warn!(
                                    correlation_id = frame.correlation_id,
                                    "rpc reader: unmatched reply"
                                );
                            }
                        }
                        FramePayload::Control(ControlMsg::Cancel) => {
                            debug!("rpc reader: received Cancel control frame");
                            cancel.cancel();
                        }
                        FramePayload::Control(ControlMsg::Shutdown) => {
                            debug!("rpc reader: received Shutdown control frame");
                            cancel.cancel();
                            return;
                        }
                        other => {
                            debug!(?other, "rpc reader: unhandled frame on worker-side");
                        }
                    },
                    Err(e) => {
                        debug!(error = %e, "rpc reader: stream closed");
                        return;
                    }
                }
            }
        }
    }
}

async fn writer_loop<W>(
    mut write_half: W,
    mut rx: mpsc::Receiver<Frame>,
    cancel: CancellationToken,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("rpc writer: cancelled");
                let _ = write_half.shutdown().await;
                return;
            }
            frame = rx.recv() => {
                let Some(frame) = frame else {
                    debug!("rpc writer: outbound channel closed");
                    let _ = write_half.shutdown().await;
                    return;
                };
                if let Err(e) = write_frame(&mut write_half, &frame).await {
                    error!(error = %e, "rpc writer: failed to write frame");
                    return;
                }
            }
        }
    }
}

// ── Compatibility stub ───────────────────────────────────────────────────────

/// Placeholder `SupervisorServices` that panics on every RPC method.
///
/// Formerly `StubRpcServices` (PR 4).  Re-exported under the old name at the
/// crate root so downstream callers do not have to change.  Used by the
/// object-safety test and by unit tests that need a `SupervisorServices` but
/// will never reach the RPC methods.
pub struct UnimplementedRpcServices {
    cancel: CancellationToken,
}

impl UnimplementedRpcServices {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_cancel(cancel: CancellationToken) -> Self {
        Self { cancel }
    }
}

impl Default for UnimplementedRpcServices {
    fn default() -> Self {
        Self::new()
    }
}

/// Historical alias preserved from PR 4.  Use [`UnimplementedRpcServices`]
/// in new code.
pub type StubRpcServices = UnimplementedRpcServices;

#[async_trait]
impl SupervisorServices for UnimplementedRpcServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn load_task(&self, _task_id: String) -> Result<Task, String> {
        unimplemented!("UnimplementedRpcServices::load_task — construct RpcServices for real RPC")
    }

    async fn execute_stage(
        &self,
        _task: &Task,
        _workspace: &Workspace,
        _role_kind: RoleKind,
        _task_run_id: &str,
        _spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        unimplemented!(
            "UnimplementedRpcServices::execute_stage — construct RpcServices for real RPC"
        )
    }

    async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
        unimplemented!("UnimplementedRpcServices::open_pr — construct RpcServices for real RPC")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The stub satisfies the trait (compile-time) and can be stored behind
    /// `Arc<dyn SupervisorServices>` (the supervisor's dispatch shape).
    #[test]
    fn stub_is_object_safe() {
        let svc: Arc<dyn SupervisorServices> = Arc::new(UnimplementedRpcServices::new());
        assert!(!svc.cancel().is_cancelled());
    }

    /// The `unimplemented!()` panic path still fires — proves the stub
    /// remains a genuine placeholder after the PR 5 rename.
    #[tokio::test]
    #[should_panic(expected = "construct RpcServices for real RPC")]
    async fn stub_load_task_panics() {
        let svc = UnimplementedRpcServices::new();
        let _ = svc.load_task("t".into()).await;
    }

    /// End-to-end load_task round-trip across an in-memory Unix socket pair.
    /// The server half runs a trivial dispatcher that echoes a canned task.
    #[tokio::test]
    async fn load_task_roundtrip_over_unixpair() {
        let (client, server) = UnixStream::pair().expect("pair");

        // Server-side dispatcher: read one request, answer with a canned task.
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::LoadTask { task_id }) => {
                    let mut task = fixture_task();
                    task.id = task_id;
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::LoadTask(Ok(task))),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let result = services.load_task("hello-task".into()).await;
        let task = result.expect("load_task ok");
        assert_eq!(task.id, "hello-task");

        // Drain the background tasks so the test exits cleanly.
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;
    }

    fn fixture_task() -> Task {
        Task {
            id: "t".into(),
            project_id: "p".into(),
            short_id: "T-1".into(),
            epic_id: None,
            title: "t".into(),
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
}
