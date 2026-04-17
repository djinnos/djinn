//! Launcher-side bincode-RPC server for [`SupervisorServices`].
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//!
//! The original [`serve_on_unix_socket`] (Phase 2 PR 5 of the retired
//! `phase2-localdocker-scaffolding.md`) lives on for tests and the in-process
//! supervisor path.  Alongside it, PR 2 lands [`serve_on_tcp`] — the same
//! dispatch loop over a TCP listener guarded by an [`AuthHello`] handshake
//! validated through an injected [`TokenValidator`].
//!
//! ## Transports
//!
//! | Transport | Handshake | Intended caller |
//! |---|---|---|
//! | `serve_on_unix_socket` | none — filesystem perms | in-process tests + legacy path |
//! | `serve_on_tcp` | `FramePayload::AuthHello { task_run_id, token }` | K8s Pod workers hitting the ClusterIP Service |
//!
//! ## Dispatch loop
//!
//! After the handshake both transports share [`dispatch_loop`], which owns
//! the per-connection reader/writer pair and routes every inbound
//! [`FramePayload::Rpc`] to the `Arc<dyn SupervisorServices>` the launcher
//! injected.  Control frames (`Cancel` / `Shutdown`) flow the same way they
//! do today.
//!
//! ## Cancellation
//!
//! Both `serve_*` entry points return a [`ServeHandle`] carrying a
//! [`CancellationToken`].  Firing that token tears the accept loop and any
//! active connection down.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use djinn_runtime::wire::{ControlMsg, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::SupervisorServices;
use super::wire::{
    AuthHelloMsg, AuthResultMsg, Frame, FramePayload, ServiceRpcRequest, ServiceRpcResponse,
};

// ── TokenValidator ───────────────────────────────────────────────────────────

/// Decoded outcome of a [`TokenValidator::validate`] call.
///
/// The real implementation lives in `djinn-k8s::token_review` (wired up
/// during PR 3) and forwards to the apiserver's `TokenReview` subresource.
/// This crate never depends on `djinn-k8s` — that would introduce a cycle
/// through `djinn-server`'s wiring layer.  Instead, the server receives any
/// impl via `Arc<V>` at boot time.
#[derive(Debug, Clone)]
pub struct TokenValidation {
    /// Whether the token was accepted.
    pub authenticated: bool,
    /// ServiceAccount username when authenticated; `None` otherwise.
    /// Used for audit logging only in v1.
    pub username: Option<String>,
}

/// Object-safe trait the TCP listener calls before accepting RPC traffic.
///
/// Impls live outside this crate:
///
/// - `djinn-k8s::KubernetesTokenValidator` (follow-up PR) — posts a
///   `TokenReview` against the in-cluster apiserver and verifies the SA
///   identity encodes the expected task-run id.
/// - [`AllowAllValidator`] / [`DenyAllValidator`] / [`ExpectedTokenValidator`]
///   — test stubs in this crate.
#[async_trait]
pub trait TokenValidator: Send + Sync + 'static {
    /// Validate `token` against `expected_task_run_id` and return a decoded
    /// [`TokenValidation`].  The return `Err(String)` is reserved for
    /// transport-level failures against the validator (e.g. apiserver
    /// unreachable); a token that simply failed authentication returns
    /// `Ok(TokenValidation { authenticated: false, .. })`.
    ///
    /// The `expected_task_run_id` comes from the worker's [`AuthHelloMsg`]
    /// payload.  Real K8s impls use it to verify that the SA identity
    /// encoded in the bearer token is scoped to the same task run — a
    /// foreign worker presenting another task-run's token is rejected.
    async fn validate(
        &self,
        token: &str,
        expected_task_run_id: &str,
    ) -> Result<TokenValidation, String>;
}

/// Trivial [`TokenValidator`] that accepts every token.  Used by the
/// unix-socket path (the filesystem is the auth barrier there) and by
/// tests.  NEVER wire this into a production server.
pub struct AllowAllValidator;

#[async_trait]
impl TokenValidator for AllowAllValidator {
    async fn validate(
        &self,
        _token: &str,
        _expected_task_run_id: &str,
    ) -> Result<TokenValidation, String> {
        Ok(TokenValidation {
            authenticated: true,
            username: Some("allow-all".into()),
        })
    }
}

/// Trivial [`TokenValidator`] that rejects every token.  Used as a safe
/// default + in contrast tests.
pub struct DenyAllValidator;

#[async_trait]
impl TokenValidator for DenyAllValidator {
    async fn validate(
        &self,
        _token: &str,
        _expected_task_run_id: &str,
    ) -> Result<TokenValidation, String> {
        Ok(TokenValidation {
            authenticated: false,
            username: None,
        })
    }
}

/// Exact-match [`TokenValidator`] for tests.  Accepts iff *both* the
/// presented bearer token and the task-run id on the [`AuthHelloMsg`]
/// match the values pinned at construction time.
///
/// Mismatched task-run id → `authenticated: false` with a descriptive
/// username slot so rejecting tests can tell the two failure modes apart.
/// Invalid token → `authenticated: false` with `username: None`.
pub struct ExpectedTokenValidator {
    pub expected_token: String,
    pub expected_task_run_id: String,
}

impl ExpectedTokenValidator {
    pub fn new(
        expected_token: impl Into<String>,
        expected_task_run_id: impl Into<String>,
    ) -> Self {
        Self {
            expected_token: expected_token.into(),
            expected_task_run_id: expected_task_run_id.into(),
        }
    }
}

#[async_trait]
impl TokenValidator for ExpectedTokenValidator {
    async fn validate(
        &self,
        token: &str,
        expected_task_run_id: &str,
    ) -> Result<TokenValidation, String> {
        if token != self.expected_token {
            return Ok(TokenValidation {
                authenticated: false,
                username: None,
            });
        }
        if expected_task_run_id != self.expected_task_run_id {
            return Ok(TokenValidation {
                authenticated: false,
                username: Some(format!(
                    "mismatched task_run_id (expected {}, got {})",
                    self.expected_task_run_id, expected_task_run_id
                )),
            });
        }
        Ok(TokenValidation {
            authenticated: true,
            username: Some(format!("expected:{}", self.expected_task_run_id)),
        })
    }
}

// ── ServeHandle ──────────────────────────────────────────────────────────────

/// Handle returned by either [`serve_on_unix_socket`] or [`serve_on_tcp`].
///
/// Dropping the handle does not automatically cancel the server.  Call
/// [`ServeHandle::cancel`] (or drop the token you passed in) to force-stop.
pub struct ServeHandle {
    /// Fire to tear the accept loop and any active connection down.
    pub cancel_server: CancellationToken,
    /// Await to join the accept loop (and the per-connection tasks it spawned).
    pub join: JoinHandle<()>,
    /// Path the socket was bound to, for the unix-socket path.  `None` on TCP.
    pub socket_path: Option<PathBuf>,
    /// Bound TCP address, for the TCP path.  `None` on the unix-socket path.
    pub bound_addr: Option<SocketAddr>,
}

impl ServeHandle {
    /// Signal the server to stop.
    pub fn cancel(&self) {
        self.cancel_server.cancel();
    }
}

// ── serve_on_unix_socket (legacy path, kept for tests) ───────────────────────

/// Bind a Unix-domain socket at `path` and spawn the accept loop.
///
/// Intended for in-process tests and the legacy launcher path.  The TCP
/// path ([`serve_on_tcp`]) is the production transport after Phase 2 K8s
/// PR 2; this entry point stays functional so `rpc_roundtrip.rs` keeps
/// exercising the dispatch without extra auth machinery.
pub async fn serve_on_unix_socket<P: AsRef<Path>>(
    path: P,
    services: Arc<dyn SupervisorServices>,
) -> io::Result<ServeHandle> {
    let socket_path = path.as_ref().to_path_buf();
    // Tolerate a stale socket from a previous run.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    info!(socket = %socket_path.display(), "SupervisorServices RPC server listening (unix)");

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
                        debug!(socket = %path_for_task.display(), "worker connected (unix)");
                        let (read_half, write_half) = stream.into_split();
                        dispatch_loop(read_half, write_half, services_arc, cancel_child.clone()).await;
                    }
                    Err(e) => {
                        error!(error = %e, "SupervisorServices accept failed (unix)");
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
        socket_path: Some(socket_path),
        bound_addr: None,
    })
}

// ── serve_on_tcp (PR 2) ──────────────────────────────────────────────────────

/// Per-task-run connection state stored on the listener.
///
/// Single entry per `task_run_id`; the listener rejects a second AuthHello
/// bearing an already-bound task_run_id.  In PR 2 the value is minimal —
/// PR 4 extends it with a reference to the outbound control channel so the
/// host-side dispatcher can push `Cancel` / `Shutdown` frames to the right
/// connection.
#[derive(Debug)]
struct ConnState {
    /// Username the validator returned — useful for audit logs.
    #[allow(dead_code)]
    username: Option<String>,
}

/// Bind a TCP listener at `addr` and spawn the accept loop.
///
/// Every accepted connection must send an [`FramePayload::AuthHello`] as
/// its first frame.  The server validates the embedded token via
/// `validator.validate(token, &hello.task_run_id)` and rejects the
/// connection on failure.  On success it records `task_run_id → ConnState`
/// and enters the shared [`dispatch_loop`] — from that point on the TCP
/// path is byte-for-byte identical to the unix-socket path.
pub async fn serve_on_tcp<V: TokenValidator>(
    addr: SocketAddr,
    services: Arc<dyn SupervisorServices>,
    validator: Arc<V>,
) -> io::Result<ServeHandle> {
    let listener = TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    info!(addr = %bound_addr, "SupervisorServices RPC server listening (tcp)");

    let cancel_server = CancellationToken::new();
    let cancel_child = cancel_server.clone();

    // Task-run id → connection state.  Guards against a second handshake
    // re-using an already-bound task_run_id within the lifetime of this
    // listener.
    let conns: Arc<Mutex<HashMap<String, ConnState>>> = Arc::new(Mutex::new(HashMap::new()));

    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel_child.cancelled() => {
                    debug!(addr = %bound_addr, "tcp server cancelled");
                    return;
                }
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, peer)) => {
                            debug!(%peer, "worker connected (tcp)");
                            // One per-connection task so a slow validator
                            // does not block other handshakes.
                            let services = services.clone();
                            let validator = validator.clone();
                            let conns = conns.clone();
                            let cancel_conn = cancel_child.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_tcp_connection(
                                    stream,
                                    services,
                                    validator,
                                    conns,
                                    cancel_conn,
                                )
                                .await
                                {
                                    warn!(%peer, error = %e, "tcp connection terminated");
                                }
                            });
                        }
                        Err(e) => {
                            error!(error = %e, "tcp accept failed");
                            // Transient errors are retried on the next iteration;
                            // there's no obvious way to partially fail.
                        }
                    }
                }
            }
        }
    });

    Ok(ServeHandle {
        cancel_server,
        join,
        socket_path: None,
        bound_addr: Some(bound_addr),
    })
}

/// Run one TCP connection: read the AuthHello, validate, register, then
/// enter the shared dispatch loop.
async fn handle_tcp_connection<V: TokenValidator>(
    stream: tokio::net::TcpStream,
    services: Arc<dyn SupervisorServices>,
    validator: Arc<V>,
    conns: Arc<Mutex<HashMap<String, ConnState>>>,
    cancel: CancellationToken,
) -> Result<(), String> {
    let (read_half, mut write_half) = stream.into_split();

    // Use a small adapter so read_frame works on the owned read half.
    // (`OwnedReadHalf` already impls `AsyncRead`, so no wrapper needed.)
    let mut read_half = read_half;

    // 1. First frame must be AuthHello.
    let first = match read_frame::<_, Frame>(&mut read_half).await {
        Ok(f) => f,
        Err(e) => {
            return Err(format!("read AuthHello: {e}"));
        }
    };
    let (task_run_id, token) = match first.payload {
        FramePayload::AuthHello(AuthHelloMsg { task_run_id, token }) => (task_run_id, token),
        other => {
            // Best-effort reject and close.
            let reject = Frame {
                correlation_id: first.correlation_id,
                payload: FramePayload::AuthResult(AuthResultMsg {
                    accepted: false,
                    error: Some("first frame must be AuthHello".into()),
                }),
            };
            let _ = write_frame(&mut write_half, &reject).await;
            let _ = write_half.shutdown().await;
            return Err(format!("handshake: expected AuthHello, got {other:?}"));
        }
    };

    // 2. Validate the bearer token.  The validator is free to interpret
    //    `task_run_id` however it wants — the real K8s impl checks the SA
    //    identity encoded in the token matches this string, `ExpectedTokenValidator`
    //    does exact-match, and `AllowAllValidator` ignores it entirely.
    let validation = match validator.validate(&token, &task_run_id).await {
        Ok(v) => v,
        Err(e) => {
            let reject = Frame {
                correlation_id: first.correlation_id,
                payload: FramePayload::AuthResult(AuthResultMsg {
                    accepted: false,
                    error: Some(format!("validator error: {e}")),
                }),
            };
            let _ = write_frame(&mut write_half, &reject).await;
            let _ = write_half.shutdown().await;
            return Err(format!("validator: {e}"));
        }
    };
    if !validation.authenticated {
        let reject = Frame {
            correlation_id: first.correlation_id,
            payload: FramePayload::AuthResult(AuthResultMsg {
                accepted: false,
                error: Some("token rejected".into()),
            }),
        };
        let _ = write_frame(&mut write_half, &reject).await;
        let _ = write_half.shutdown().await;
        return Err(format!("token rejected for task_run_id={task_run_id}"));
    }

    // 3. Register the connection; reject double-binds.
    {
        let mut map = conns.lock().await;
        if map.contains_key(&task_run_id) {
            let reject = Frame {
                correlation_id: first.correlation_id,
                payload: FramePayload::AuthResult(AuthResultMsg {
                    accepted: false,
                    error: Some(format!(
                        "task_run_id {task_run_id} already bound to another connection"
                    )),
                }),
            };
            let _ = write_frame(&mut write_half, &reject).await;
            let _ = write_half.shutdown().await;
            return Err(format!("duplicate handshake for {task_run_id}"));
        }
        map.insert(
            task_run_id.clone(),
            ConnState {
                username: validation.username.clone(),
            },
        );
    }

    // 4. Ack the handshake.
    let ack = Frame {
        correlation_id: first.correlation_id,
        payload: FramePayload::AuthResult(AuthResultMsg {
            accepted: true,
            error: None,
        }),
    };
    if let Err(e) = write_frame(&mut write_half, &ack).await {
        conns.lock().await.remove(&task_run_id);
        return Err(format!("write AuthResult ack: {e}"));
    }
    info!(%task_run_id, username = ?validation.username, "tcp handshake accepted");

    // 5. Enter the shared dispatch loop.
    dispatch_loop(read_half, write_half, services, cancel).await;

    // 6. Deregister.
    conns.lock().await.remove(&task_run_id);
    Ok(())
}

// ── Shared dispatch loop ─────────────────────────────────────────────────────

/// Drive the read/write pair for one post-handshake connection.
///
/// Parameterised on any split `AsyncRead` + `AsyncWrite` so it works over
/// both the unix-socket halves and the TCP halves without duplicating the
/// reader/writer boilerplate.
async fn dispatch_loop<R, W>(
    read_half: R,
    write_half: W,
    services: Arc<dyn SupervisorServices>,
    cancel: CancellationToken,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (reply_tx, reply_rx) = mpsc::channel::<Frame>(64);

    let writer = tokio::spawn(writer_loop(write_half, reply_rx, cancel.clone()));
    reader_loop(read_half, services, reply_tx, cancel.clone()).await;

    // Closing `reply_tx` drains the writer naturally.
    let _ = writer.await;
}

async fn reader_loop<R>(
    mut read_half: R,
    services: Arc<dyn SupervisorServices>,
    reply_tx: mpsc::Sender<Frame>,
    cancel: CancellationToken,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
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
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;

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
            unimplemented!("not exercised in server tests")
        }

        async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
            unimplemented!("not exercised in server tests")
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
        let (rpc, bg) = super::super::rpc::RpcServices::connect_unix(&sock, client_cancel.clone())
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

    /// A TCP connection that presents a token the validator rejects MUST
    /// receive an AuthResult { accepted: false } frame and then see the
    /// server close the stream.
    #[tokio::test]
    async fn serve_on_tcp_rejects_bad_token() {
        let services: Arc<dyn SupervisorServices> = Arc::new(FakeServices {
            cancel: CancellationToken::new(),
            canned_task_id: "never-reached".into(),
        });
        // ExpectedTokenValidator rejects every token that does not match.
        let validator = Arc::new(ExpectedTokenValidator::new("good-token", "run-denied"));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = serve_on_tcp(addr, services, validator)
            .await
            .expect("bind tcp");
        let bound = handle.bound_addr.expect("bound addr");

        // Dial and present an AuthHello with a bogus token.
        let mut stream = TcpStream::connect(bound).await.expect("connect");
        let hello = Frame {
            correlation_id: 1,
            payload: FramePayload::AuthHello(AuthHelloMsg {
                task_run_id: "run-denied".into(),
                token: "not-a-real-token".into(),
            }),
        };
        write_frame(&mut stream, &hello).await.expect("write hello");

        // Expect an AuthResult reject.
        let reply: Frame = read_frame(&mut stream).await.expect("read reply");
        match reply.payload {
            FramePayload::AuthResult(AuthResultMsg { accepted, .. }) => {
                assert!(!accepted, "expected reject");
            }
            other => panic!("unexpected: {other:?}"),
        }

        // After the reject the server shuts down its write half; a read
        // should return EOF (0 bytes).
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).await.expect("read after close");
        assert_eq!(n, 0, "expected EOF after reject");

        handle.cancel();
        let _ = handle.join.await;
    }

    /// A TCP connection whose AuthHello presents the correct token but the
    /// *wrong* task-run id MUST be rejected.  This proves the
    /// `expected_task_run_id` validator argument is enforced separately from
    /// the token bytes.
    #[tokio::test]
    async fn serve_on_tcp_rejects_mismatched_task_run_id() {
        let services: Arc<dyn SupervisorServices> = Arc::new(FakeServices {
            cancel: CancellationToken::new(),
            canned_task_id: "never-reached".into(),
        });
        let validator = Arc::new(ExpectedTokenValidator::new(
            "matched-token",
            "run-expected",
        ));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = serve_on_tcp(addr, services, validator)
            .await
            .expect("bind tcp");
        let bound = handle.bound_addr.expect("bound addr");

        let mut stream = TcpStream::connect(bound).await.expect("connect");
        let hello = Frame {
            correlation_id: 1,
            payload: FramePayload::AuthHello(AuthHelloMsg {
                // Right token, WRONG task_run_id.
                task_run_id: "run-imposter".into(),
                token: "matched-token".into(),
            }),
        };
        write_frame(&mut stream, &hello).await.expect("write hello");

        let reply: Frame = read_frame(&mut stream).await.expect("read reply");
        match reply.payload {
            FramePayload::AuthResult(AuthResultMsg { accepted, .. }) => {
                assert!(!accepted, "expected mismatched-id reject");
            }
            other => panic!("unexpected: {other:?}"),
        }

        // EOF after reject.
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).await.expect("read after close");
        assert_eq!(n, 0);

        handle.cancel();
        let _ = handle.join.await;
    }

    /// A TCP connection with an accepted token MUST be able to round-trip
    /// a LoadTask through the shared dispatch loop.
    #[tokio::test]
    async fn serve_on_tcp_accepts_valid_token_and_routes_rpc() {
        let services: Arc<dyn SupervisorServices> = Arc::new(FakeServices {
            cancel: CancellationToken::new(),
            canned_task_id: "tcp-task-1".into(),
        });
        let validator = Arc::new(AllowAllValidator);
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = serve_on_tcp(addr, services, validator)
            .await
            .expect("bind tcp");
        let bound = handle.bound_addr.expect("bound addr");

        // Dial, handshake, then go through RpcServices for the round-trip.
        let mut stream = TcpStream::connect(bound).await.expect("connect");
        let hello = Frame {
            correlation_id: 1,
            payload: FramePayload::AuthHello(AuthHelloMsg {
                task_run_id: "tcp-run-1".into(),
                token: "any-token".into(),
            }),
        };
        write_frame(&mut stream, &hello).await.expect("write hello");
        let reply: Frame = read_frame(&mut stream).await.expect("read ack");
        match reply.payload {
            FramePayload::AuthResult(AuthResultMsg { accepted: true, .. }) => {}
            other => panic!("expected accepted auth, got {other:?}"),
        }

        // Now the stream is in the shared dispatch loop.  Hand it to
        // `RpcServices::from_stream` and round-trip a load_task.
        let cancel = CancellationToken::new();
        let (rpc, bg) = super::super::rpc::RpcServices::from_stream(stream, cancel.clone());
        let task = rpc
            .load_task("tcp-task-1".into())
            .await
            .expect("rpc load_task");
        assert_eq!(task.id, "tcp-task-1");

        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        handle.cancel();
        let _ = handle.join.await;
    }
}
