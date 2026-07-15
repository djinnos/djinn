// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
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
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use djinn_core::models::{Task, TaskRunStatus};
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
    AuthHelloMsg, AuthResultMsg, Frame, FramePayload, SerializableCreateTaskRunParams,
    ServiceRpcRequest, ServiceRpcResponse,
};
use crate::{
    BranchPublicationResult, RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec,
};

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
    io::Error::other(msg.into())
}

/// Per-attempt backoff (in milliseconds) for [`RpcServices::connect_tcp`]'s
/// dial-retry loop. There is one sleep entry per retry, so the total connect
/// budget is `CONNECT_BACKOFF_MS.iter().sum()` and the attempt count is
/// `CONNECT_BACKOFF_MS.len() + 1` (the initial dial plus one per entry).
///
/// Sized to survive a rolling restart of djinn-server. A `helm upgrade` roll
/// of the server takes ~20-30s, during which its RPC listener is briefly
/// unreachable. Any task-run pod that starts inside that window must keep
/// dialing rather than exit 1 — an early exit strands a `pending`
/// task_attempt that blocks the (task, role) dispatch until the orphan reaper
/// fires (observed twice in production). The schedule ramps quickly for the
/// common launcher-boot race (sub-second), then holds at a 10s cap for the
/// long tail; the entries below sum to ~88.7s, covering a server roll with
/// comfortable margin.
const CONNECT_BACKOFF_MS: &[u64] = &[
    100, 200, 400, 1000, 2000, 5000, 10000, 10000, 10000, 10000, 10000, 10000, 10000, 10000,
];

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
        addr: impl tokio::net::ToSocketAddrs + std::fmt::Display,
        task_run_id: String,
        token: String,
        cancel: CancellationToken,
    ) -> Result<(Arc<Self>, RpcBackgroundTasks), ConnectTcpError> {
        // Retry the TCP dial with exponential backoff so the worker tolerates
        // launcher races AND a server rolling restart: the dispatch path can
        // create the worker Job within milliseconds of the launcher boot,
        // before the launcher's TCP listener on :8443 has bound, and a
        // `helm upgrade` roll of djinn-server (~20-30s) makes the RPC listener
        // transiently unreachable for any pod that starts inside that window.
        // Without a budget that spans the roll, a single "Connection refused"
        // kills the task-run (Job backoff_limit=0 means no K8s-level retry)
        // and strands a `pending` task_attempt that blocks (task, role)
        // dispatch until the orphan reaper fires. See [`CONNECT_BACKOFF_MS`]
        // for the schedule and the ~88.7s total budget.
        let mut stream = {
            let backoff_ms = CONNECT_BACKOFF_MS;
            let total_attempts = backoff_ms.len() + 1;
            let budget_secs = backoff_ms.iter().sum::<u64>() / 1000;
            let mut attempt = 0usize;
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(ConnectTcpError::Io(io_other(
                            "connect_tcp cancelled before dial succeeded".to_string(),
                        )));
                    }
                    res = TcpStream::connect(&addr) => match res {
                        Ok(s) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
                                  || e.kind() == std::io::ErrorKind::TimedOut
                                  || e.kind() == std::io::ErrorKind::NotFound // DNS not-resolvable yet
                            => {
                            if attempt >= backoff_ms.len() {
                                return Err(ConnectTcpError::Io(io_other(format!(
                                    "connect_tcp: launcher unreachable after {total_attempts} attempts ({budget_secs}s budget): {e}",
                                ))));
                            }
                            let delay = backoff_ms[attempt];
                            attempt += 1;
                            tracing::warn!(
                                %addr,
                                attempt,
                                total_attempts,
                                budget_secs,
                                next_retry_ms = delay,
                                error = %e,
                                "connect_tcp: launcher not yet listening; retrying (tolerates a server roll)",
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        }
                        Err(e) => return Err(ConnectTcpError::Io(e)),
                    }
                }
            }
        };
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
            FramePayload::AuthResult(AuthResultMsg { accepted: true, .. }) => {
                info!(%task_run_id, "tcp auth accepted");
            }
            FramePayload::AuthResult(AuthResultMsg {
                accepted: false,
                error,
            }) => {
                let reason = error.unwrap_or("token rejected".into());
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

    /// Push an out-of-band [`WorkerEvent`] onto the outbound mpsc channel so
    /// the writer task frames it as a [`FramePayload::Event`] on the shared
    /// RPC socket.
    ///
    /// The worker uses this to ship its terminal
    /// [`djinn_runtime::TaskRunReport`] back to the launcher via
    /// [`WorkerEvent::TerminalReport`]. Events carry `correlation_id = 0` —
    /// they travel out-of-band of any request/reply round-trip — matching
    /// the convention already used by `FramePayload::Control`.
    ///
    /// Returns an error only if the outbound channel has been closed (writer
    /// task exited).  Callers should treat that as "connection lost" and
    /// give up on further emits.
    pub async fn emit_event(&self, event: djinn_runtime::WorkerEvent) -> Result<(), String> {
        let frame = Frame {
            correlation_id: 0,
            payload: FramePayload::Event(event),
        };
        self.tx
            .send(frame)
            .await
            .map_err(|_| "rpc writer task dropped — cannot emit event".to_string())
    }

    /// Allocate a fresh correlation id, send the request, and await the
    /// matching reply.  Returns a transport-level error if the socket closed
    /// before a reply arrived or the response variant did not match the
    /// request shape.
    ///
    /// The reply wait is cancel-aware: if the shared `CancellationToken`
    /// fires (e.g. the host sent `Control(Cancel)` and the reader loop
    /// flipped the token before the writer could deliver this request, or
    /// the writer raced ahead but the reader exited before the reply
    /// arrived), the await unparks with a typed transport error instead of
    /// hanging forever waiting on a oneshot whose sender will never be
    /// driven.  Without this guard, a `Control(Cancel)` arriving mid-
    /// roundtrip would deadlock the worker: the writer's `cancelled()`
    /// branch tears down the write half and drops `rx`, but the in-flight
    /// `oneshot::Receiver` still sees the matching sender alive in
    /// `pending`, so `rx.await` waits forever.
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

        let mut rx = rx;
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                // The reply may already be sitting in the oneshot — the
                // reader delivers it and can fire `cancel` on the very next
                // iteration when the socket EOFs (transport-death wind-down).
                // A delivered reply beats the cancellation.
                if let Ok(reply) = rx.try_recv() {
                    return Ok(reply);
                }
                // Best-effort: pop our oneshot so the reader-loop drain
                // doesn't try to deliver a reply that's already abandoned.
                self.pending.lock().await.remove(&correlation_id);
                Err(format!(
                    "rpc roundtrip cancelled before reply (correlation_id={correlation_id})"
                ))
            }
            reply = &mut rx => {
                reply.map_err(|_| {
                    // Reader task dropped the oneshot without replying —
                    // usually because the socket closed before the reply
                    // arrived.
                    format!("rpc reply channel closed (correlation_id={correlation_id})")
                })
            }
        }
    }
}

#[async_trait]
impl SupervisorServices for RpcServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn report_stage_step(&self, step: &'static str) -> Result<(), String> {
        // Ship the marker out-of-band on the shared frame channel; the host's
        // reader_loop lowers it to a `StreamEvent::StageStep`. Best-effort —
        // a dropped writer just means the connection is already gone.
        self.emit_event(djinn_runtime::WorkerEvent::StageStep {
            step: step.to_string(),
        })
        .await
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
            Ok(ServiceRpcResponse::Err(e)) => Err(StageError::Setup(format!("rpc transport: {e}"))),
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
                provider_failure: None,
                reason: format!("rpc transport: {e}"),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            Ok(other) => TaskRunOutcome::Failed {
                stage: "open_pr".into(),
                provider_failure: None,
                reason: format!("rpc protocol: unexpected reply {other:?}"),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
            Err(e) => TaskRunOutcome::Failed {
                stage: "open_pr".into(),
                provider_failure: None,
                reason: e,
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
        }
    }

    async fn create_task_run(&self, params: SerializableCreateTaskRunParams) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::CreateTaskRun { params })
            .await
        {
            Ok(ServiceRpcResponse::CreateTaskRun(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn update_task_run_status(
        &self,
        run_id: String,
        status: TaskRunStatus,
    ) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::UpdateTaskRunStatus { run_id, status })
            .await
        {
            Ok(ServiceRpcResponse::UpdateTaskRunStatus(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn get_model_context_window(&self, model_id: String) -> Result<i64, String> {
        match self
            .roundtrip(ServiceRpcRequest::GetModelContextWindow { model_id })
            .await
        {
            Ok(ServiceRpcResponse::GetModelContextWindow(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn get_provider_base_url(&self, catalog_provider_id: String) -> Result<String, String> {
        match self
            .roundtrip(ServiceRpcRequest::GetProviderBaseUrl {
                catalog_provider_id,
            })
            .await
        {
            Ok(ServiceRpcResponse::GetProviderBaseUrl(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        match self.roundtrip(ServiceRpcRequest::PickAnyDefaultModel).await {
            Ok(ServiceRpcResponse::PickAnyDefaultModel(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn create_session(
        &self,
        params: crate::services::SerializableCreateSessionParams,
    ) -> Result<djinn_core::models::SessionRecord, String> {
        match self
            .roundtrip(ServiceRpcRequest::CreateSession { params })
            .await
        {
            Ok(ServiceRpcResponse::CreateSession(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn publish_session_message(
        &self,
        session_id: String,
        task_id: String,
        agent_type: String,
        message: serde_json::Value,
    ) -> Result<(), String> {
        // Opaque JSON encode for bincode safety — `serde_json::Value`'s
        // untagged-enum internals trip `DeserializeAnyNotSupported`.
        let message = serde_json::to_string(&message).unwrap_or("null".to_string());
        match self
            .roundtrip(ServiceRpcRequest::PublishSessionMessage {
                session_id,
                task_id,
                agent_type,
                message,
            })
            .await
        {
            Ok(ServiceRpcResponse::PublishSessionMessage(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn get_environment_config(
        &self,
        project_id: String,
    ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
        match self
            .roundtrip(ServiceRpcRequest::GetEnvironmentConfig { project_id })
            .await
        {
            Ok(ServiceRpcResponse::GetEnvironmentConfig(Ok(payload))) => {
                serde_json::from_str::<djinn_stack::environment::EnvironmentConfig>(&payload)
                    .map_err(|e| format!("rpc decode get_environment_config reply: {e}"))
            }
            Ok(ServiceRpcResponse::GetEnvironmentConfig(Err(e))) => Err(e),
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn invoke_llm(
        &self,
        model_id: String,
        conversation: djinn_provider::message::Conversation,
        tools: Vec<serde_json::Value>,
        tool_choice: Option<djinn_provider::provider::ToolChoice>,
    ) -> Result<djinn_provider::provider::LlmResponse, String> {
        // Opaque JSON encode — `Conversation` carries `ContentBlock`
        // (internally-tagged + `serde_json::Value`) and `MessageMeta`
        // with `skip_serializing_if`, both bincode-fatal. `tools` is a
        // raw `Vec<Value>`. Both are JSON-stringified for the wire.
        let conversation_str = serde_json::to_string(&conversation)
            .map_err(|e| format!("encode conversation for rpc: {e}"))?;
        let tools_str =
            serde_json::to_string(&tools).map_err(|e| format!("encode tools for rpc: {e}"))?;
        match self
            .roundtrip(ServiceRpcRequest::InvokeLlm {
                model_id,
                conversation: conversation_str,
                tools: tools_str,
                tool_choice,
            })
            .await
        {
            Ok(ServiceRpcResponse::InvokeLlm(Ok(payload))) => {
                serde_json::from_str::<djinn_provider::provider::LlmResponse>(&payload)
                    .map_err(|e| format!("rpc decode invoke_llm reply: {e}"))
            }
            Ok(ServiceRpcResponse::InvokeLlm(Err(e))) => Err(e),
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_session_status(
        &self,
        session_id: String,
        status: djinn_core::models::SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
        parked_reason: Option<String>,
    ) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::UpdateSessionStatus {
                session_id,
                status,
                tokens_in,
                tokens_out,
                cache_read,
                cache_write,
                parked_reason,
            })
            .await
        {
            Ok(ServiceRpcResponse::UpdateSessionStatus(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn flush_session_tokens(
        &self,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::FlushSessionTokens {
                session_id,
                tokens_in,
                tokens_out,
                cache_read,
                cache_write,
            })
            .await
        {
            Ok(ServiceRpcResponse::FlushSessionTokens(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn emit_djinn_event(
        &self,
        event: crate::services::SerializableDjinnEvent,
    ) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::EmitDjinnEvent { event })
            .await
        {
            Ok(ServiceRpcResponse::EmitDjinnEvent(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn tool_github_search(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        // Opaque JSON encode — `serde_json::Map` contains `Value`s whose
        // untagged-enum internals trip bincode.
        let arguments = serde_json::to_string(&arguments).unwrap_or("{}".to_string());
        match self
            .roundtrip(ServiceRpcRequest::ToolGithubSearch {
                project_id,
                arguments,
            })
            .await
        {
            Ok(ServiceRpcResponse::ToolGithubSearch(Ok(payload))) => {
                serde_json::from_str::<serde_json::Value>(&payload)
                    .map_err(|e| format!("rpc decode tool_github_search reply: {e}"))
            }
            Ok(ServiceRpcResponse::ToolGithubSearch(Err(e))) => Err(e),
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn tool_github_fetch_file(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let arguments = serde_json::to_string(&arguments).unwrap_or("{}".to_string());
        match self
            .roundtrip(ServiceRpcRequest::ToolGithubFetchFile {
                project_id,
                arguments,
            })
            .await
        {
            Ok(ServiceRpcResponse::ToolGithubFetchFile(Ok(payload))) => {
                serde_json::from_str::<serde_json::Value>(&payload)
                    .map_err(|e| format!("rpc decode tool_github_fetch_file reply: {e}"))
            }
            Ok(ServiceRpcResponse::ToolGithubFetchFile(Err(e))) => Err(e),
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn tool_ci_job_log(
        &self,
        session_task_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let arguments = serde_json::to_string(&arguments).unwrap_or("{}".to_string());
        match self
            .roundtrip(ServiceRpcRequest::ToolCiJobLog {
                session_task_id,
                arguments,
            })
            .await
        {
            Ok(ServiceRpcResponse::ToolCiJobLog(Ok(payload))) => {
                serde_json::from_str::<serde_json::Value>(&payload)
                    .map_err(|e| format!("rpc decode tool_ci_job_log reply: {e}"))
            }
            Ok(ServiceRpcResponse::ToolCiJobLog(Err(e))) => Err(e),
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn touch_activity(&self, task_id: String) -> Result<(), String> {
        // Fire-and-forget shape: we still round-trip (so the host can
        // ack reception), but a transport flake is swallowed by the
        // reply-loop caller via `.unwrap_or_else(..)`. Mirrors
        // `publish_session_message`'s pattern minus the opaque-JSON
        // encoding (no payload).
        match self
            .roundtrip(ServiceRpcRequest::TouchActivity { task_id })
            .await
        {
            Ok(ServiceRpcResponse::TouchActivity(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn transition_task(
        &self,
        task_id: String,
        action: String,
        reason: Option<String>,
    ) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::TransitionTask {
                task_id,
                action,
                reason,
            })
            .await
        {
            Ok(ServiceRpcResponse::TransitionTask(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn record_arbiter_decision(
        &self,
        task_id: String,
        decision: String,
        evidence_json: String,
    ) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::RecordArbiterDecision {
                task_id,
                decision,
                evidence_json,
            })
            .await
        {
            Ok(ServiceRpcResponse::RecordArbiterDecision(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn start_monitored_reopen(
        &self,
        task_id: String,
        directive: String,
        verification_command: String,
        exclude_models: Vec<String>,
    ) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::StartMonitoredReopen {
                task_id,
                directive,
                verification_command,
                exclude_models,
            })
            .await
        {
            Ok(ServiceRpcResponse::StartMonitoredReopen(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn complete_monitored_reopen(&self, task_id: String) -> Result<(), String> {
        match self
            .roundtrip(ServiceRpcRequest::CompleteMonitoredReopen { task_id })
            .await
        {
            Ok(ServiceRpcResponse::CompleteMonitoredReopen(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn record_arbiter_session_termination(
        &self,
        task_id: String,
        is_infra_failure: bool,
    ) -> Result<bool, String> {
        match self
            .roundtrip(ServiceRpcRequest::RecordArbiterSessionTermination {
                task_id,
                is_infra_failure,
            })
            .await
        {
            Ok(ServiceRpcResponse::RecordArbiterSessionTermination(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }

    async fn publish_branch_to_github(
        &self,
        spec: &TaskRunSpec,
        task: &Task,
    ) -> BranchPublicationResult {
        let req = ServiceRpcRequest::PublishBranchToGithub {
            spec: spec.clone(),
            task: task.clone(),
        };
        match self.roundtrip(req).await {
            Ok(ServiceRpcResponse::PublishBranchToGithub(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => BranchPublicationResult {
                success: false,
                pushed_sha: None,
                mirror_head: String::new(),
                attempted_github_head: String::new(),
                pr_branch_existed: false,
                error_class: Some("rpc_transport".into()),
                error_message: Some(format!("rpc transport: {e}")),
            },
            Ok(other) => BranchPublicationResult {
                success: false,
                pushed_sha: None,
                mirror_head: String::new(),
                attempted_github_head: String::new(),
                pr_branch_existed: false,
                error_class: Some("rpc_protocol".into()),
                error_message: Some(format!("rpc protocol: unexpected reply {other:?}")),
            },
            Err(e) => BranchPublicationResult {
                success: false,
                pushed_sha: None,
                mirror_head: String::new(),
                attempted_github_head: String::new(),
                pr_branch_existed: false,
                error_class: Some("rpc_transport".into()),
                error_message: Some(e),
            },
        }
    }

    async fn plan_memory_intents(
        &self,
        request: crate::services::wire::AttributedPlannerRequest,
    ) -> Result<crate::services::wire::PlannerAttemptResult, String> {
        match self
            .roundtrip(ServiceRpcRequest::PlanMemoryIntents { request })
            .await
        {
            Ok(ServiceRpcResponse::PlanMemoryIntents(result)) => result,
            Ok(ServiceRpcResponse::Err(e)) => Err(format!("rpc transport: {e}")),
            Ok(other) => Err(format!("rpc protocol: unexpected reply {other:?}")),
            Err(e) => Err(e),
        }
    }
}

// ── Reader / writer loops ────────────────────────────────────────────────────

async fn reader_loop<R>(mut read_half: R, pending: PendingMap, cancel: CancellationToken)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    // Loop until cancellation or socket close, then drain `pending` on the
    // way out so blocked `roundtrip()` callers fail with a transport error
    // instead of hanging on a `oneshot::Receiver` whose `Sender` will never
    // be polled.  Without this drain, a host-initiated `Control(Cancel)`
    // arriving mid-roundtrip would deadlock the worker: the reader exits
    // on the next iteration's `biased; cancelled()` branch, the writer
    // tears down its half, and the in-flight `rx.await` in `roundtrip`
    // waits forever for a reply that can no longer arrive.
    let outcome: Result<(), ()> = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!("rpc reader: cancelled");
                break Ok(());
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
                            break Ok(());
                        }
                        other => {
                            debug!(?other, "rpc reader: unhandled frame on worker-side");
                        }
                    },
                    Err(e) => {
                        // Transport death is a terminal event for the worker:
                        // there is no reconnect (a restarted host has a new
                        // address), so without RPC the run can neither receive
                        // instructions nor deliver results. Fire the cancel
                        // token so the worker winds down through the same
                        // graceful path as SIGTERM instead of idling forever
                        // as an orphan pod that holds node capacity. (Seen
                        // 2026-06-11: a server deploy interrupted 3 runs but
                        // their pods kept running for 50+ min, wedging
                        // scheduling on the single-node VPS.)
                        warn!(error = %e, "rpc reader: stream closed; cancelling worker");
                        cancel.cancel();
                        break Ok(());
                    }
                }
            }
        }
    };
    let _ = outcome;

    // Drain every pending oneshot so any `roundtrip()` parked on `rx.await`
    // wakes up with a transport error rather than hanging indefinitely.
    let drained: Vec<(u64, _)> = {
        let mut map = pending.lock().await;
        map.drain().collect()
    };
    for (correlation_id, tx) in drained {
        // Dropping `tx` (the oneshot sender) closes the channel so the
        // matching `rx.await` returns `Err`; `roundtrip()` already maps
        // that to a "rpc reply channel closed" transport error.
        let _ = tx;
        debug!(
            correlation_id,
            "rpc reader: dropped pending reply on shutdown"
        );
    }
}

async fn writer_loop<W>(mut write_half: W, mut rx: mpsc::Receiver<Frame>, cancel: CancellationToken)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Unlike the reader, the writer does NOT exit eagerly on cancel.  On a
    // host-initiated `Control(Cancel)` the supervisor still needs to
    // round-trip its terminal `update_task_run_status` frame and (if the
    // RPC connection is still healthy) emit a `WorkerEvent::TerminalReport`
    // before tearing the connection down.  An eager `cancelled()` branch
    // here would race that path: the writer would shut the write half
    // *before* the supervisor's frames landed on `rx`, causing the
    // terminal status update to be silently dropped from the host's
    // perspective.
    //
    // Instead the writer exits when `rx` closes — i.e. when every
    // `Arc<RpcServices>` (and therefore every clone of `self.tx`) has
    // been dropped.  Worker `main.rs` enforces that ordering on the
    // happy-path teardown: drop the `Arc`s, await this task, *then* fire
    // `cancel.cancel()` so the reader can exit too.  The cancel-driven
    // teardown reaches the writer transitively, by the same drop chain.
    //
    // `cancel` is only ever FIRED here (on transport death), never awaited.
    loop {
        let Some(frame) = rx.recv().await else {
            debug!("rpc writer: outbound channel closed");
            let _ = write_half.shutdown().await;
            return;
        };
        if let Err(e) = write_frame(&mut write_half, &frame).await {
            // Same rationale as the reader's stream-closed branch: a failed
            // write means the host is gone and there is no reconnect, so the
            // worker must wind down rather than orphan itself.
            error!(error = %e, "rpc writer: failed to write frame; cancelling worker");
            cancel.cancel();
            return;
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

    async fn create_task_run(
        &self,
        _params: SerializableCreateTaskRunParams,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::create_task_run — construct RpcServices for real RPC"
        )
    }

    async fn update_task_run_status(
        &self,
        _run_id: String,
        _status: TaskRunStatus,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::update_task_run_status — construct RpcServices for real RPC"
        )
    }

    async fn get_model_context_window(&self, _model_id: String) -> Result<i64, String> {
        unimplemented!(
            "UnimplementedRpcServices::get_model_context_window — construct RpcServices for real RPC"
        )
    }

    async fn get_provider_base_url(&self, _catalog_provider_id: String) -> Result<String, String> {
        unimplemented!(
            "UnimplementedRpcServices::get_provider_base_url — construct RpcServices for real RPC"
        )
    }

    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        unimplemented!(
            "UnimplementedRpcServices::pick_any_default_model — construct RpcServices for real RPC"
        )
    }

    async fn create_session(
        &self,
        _params: crate::services::SerializableCreateSessionParams,
    ) -> Result<djinn_core::models::SessionRecord, String> {
        unimplemented!(
            "UnimplementedRpcServices::create_session — construct RpcServices for real RPC"
        )
    }

    async fn publish_session_message(
        &self,
        _session_id: String,
        _task_id: String,
        _agent_type: String,
        _message: serde_json::Value,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::publish_session_message — construct RpcServices for real RPC"
        )
    }

    async fn get_environment_config(
        &self,
        _project_id: String,
    ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
        unimplemented!(
            "UnimplementedRpcServices::get_environment_config — construct RpcServices for real RPC"
        )
    }

    async fn invoke_llm(
        &self,
        _model_id: String,
        _conversation: djinn_provider::message::Conversation,
        _tools: Vec<serde_json::Value>,
        _tool_choice: Option<djinn_provider::provider::ToolChoice>,
    ) -> Result<djinn_provider::provider::LlmResponse, String> {
        unimplemented!("UnimplementedRpcServices::invoke_llm — construct RpcServices for real RPC")
    }

    #[allow(clippy::too_many_arguments)]
    async fn update_session_status(
        &self,
        _session_id: String,
        _status: djinn_core::models::SessionStatus,
        _tokens_in: i64,
        _tokens_out: i64,
        _cache_read: i64,
        _cache_write: i64,
        _parked_reason: Option<String>,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::update_session_status — construct RpcServices for real RPC"
        )
    }

    async fn emit_djinn_event(
        &self,
        _event: crate::services::SerializableDjinnEvent,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::emit_djinn_event — construct RpcServices for real RPC"
        )
    }

    async fn tool_github_search(
        &self,
        _project_id: Option<String>,
        _arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!(
            "UnimplementedRpcServices::tool_github_search — construct RpcServices for real RPC"
        )
    }

    async fn tool_github_fetch_file(
        &self,
        _project_id: Option<String>,
        _arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!(
            "UnimplementedRpcServices::tool_github_fetch_file — construct RpcServices for real RPC"
        )
    }

    async fn tool_ci_job_log(
        &self,
        _session_task_id: Option<String>,
        _arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!(
            "UnimplementedRpcServices::tool_ci_job_log — construct RpcServices for real RPC"
        )
    }

    async fn touch_activity(&self, _task_id: String) -> Result<(), String> {
        // No-op rather than panic: `touch_activity` is fire-and-forget on
        // the production reply_loop side (errors are swallowed), so panicking
        // here only breaks unrelated tests without catching any real bug.
        Ok(())
    }

    async fn transition_task(
        &self,
        _task_id: String,
        _action: String,
        _reason: Option<String>,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::transition_task — construct RpcServices for real RPC"
        )
    }

    async fn record_arbiter_decision(
        &self,
        _task_id: String,
        _decision: String,
        _evidence_json: String,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::record_arbiter_decision — construct RpcServices for real RPC"
        )
    }

    async fn start_monitored_reopen(
        &self,
        _task_id: String,
        _directive: String,
        _verification_command: String,
        _exclude_models: Vec<String>,
    ) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::start_monitored_reopen — construct RpcServices for real RPC"
        )
    }

    async fn complete_monitored_reopen(&self, _task_id: String) -> Result<(), String> {
        unimplemented!(
            "UnimplementedRpcServices::complete_monitored_reopen — construct RpcServices for real RPC"
        )
    }

    async fn record_arbiter_session_termination(
        &self,
        _task_id: String,
        _is_infra_failure: bool,
    ) -> Result<bool, String> {
        unimplemented!(
            "UnimplementedRpcServices::record_arbiter_session_termination — construct RpcServices for real RPC"
        )
    }

    async fn publish_branch_to_github(
        &self,
        _spec: &TaskRunSpec,
        _task: &Task,
    ) -> BranchPublicationResult {
        unimplemented!(
            "UnimplementedRpcServices::publish_branch_to_github — construct RpcServices for real RPC"
        )
    }

    async fn plan_memory_intents(
        &self,
        _request: crate::services::wire::AttributedPlannerRequest,
    ) -> Result<crate::services::wire::PlannerAttemptResult, String> {
        unimplemented!(
            "UnimplementedRpcServices::plan_memory_intents — construct RpcServices for real RPC"
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The connect-retry budget must span a server rolling restart. A
    /// `helm upgrade` roll of djinn-server takes ~20-30s during which its RPC
    /// listener is unreachable; a task-run pod that starts in that window has
    /// to keep dialing rather than exit and strand a `pending` attempt. Assert
    /// the total budget clears the roll with margin and that the tail holds at
    /// a 10s cap (fast ramp for the launcher-boot race, patient tail for a
    /// roll).
    #[test]
    fn connect_backoff_budget_covers_a_server_roll() {
        let total_ms: u64 = CONNECT_BACKOFF_MS.iter().sum();
        // A server roll is ~20-30s; require >=60s so a pod that starts at the
        // very beginning of the roll still outlasts it comfortably.
        assert!(
            total_ms >= 60_000,
            "connect budget {total_ms}ms must exceed a ~30s server roll with margin",
        );
        // The tail must be capped so we keep retrying at a steady cadence
        // instead of ballooning to minute-long sleeps.
        let cap = *CONNECT_BACKOFF_MS.iter().max().expect("non-empty schedule");
        assert_eq!(cap, 10_000, "backoff tail should hold at a 10s cap");
        assert_eq!(
            *CONNECT_BACKOFF_MS.last().expect("non-empty schedule"),
            10_000,
            "final backoff entry should sit at the 10s cap",
        );
    }

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

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;
    }

    /// Round-trip a `create_task_run` RPC through an in-memory Unix socket
    /// pair.  The server half asserts the params shape and answers Ok(()).
    #[tokio::test]
    async fn create_task_run_roundtrip_over_unixpair() {
        let (client, server) = UnixStream::pair().expect("pair");

        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::CreateTaskRun { params }) => {
                    assert_eq!(params.id, "run-create-rt");
                    assert_eq!(params.project_id, "p1");
                    assert_eq!(params.task_id, "t1");
                    assert_eq!(params.trigger_type, "new_task");
                    assert_eq!(params.status.as_deref(), Some("running"));
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::CreateTaskRun(Ok(()))),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let params = SerializableCreateTaskRunParams {
            id: "run-create-rt".into(),
            project_id: "p1".into(),
            task_id: "t1".into(),
            trigger_type: "new_task".into(),
            status: Some("running".into()),
            workspace_path: None,
            mirror_ref: None,
        };
        services
            .create_task_run(params)
            .await
            .expect("create_task_run ok");

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;
    }

    /// Round-trip an `update_task_run_status` RPC through an in-memory Unix
    /// socket pair.  Exercises both the Ok and Err reply legs.
    #[tokio::test]
    async fn update_task_run_status_roundtrip_over_unixpair() {
        // ── leg 1: Ok ───────────────────────────────────────────────────
        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::UpdateTaskRunStatus { run_id, status }) => {
                    assert_eq!(run_id, "run-update-rt");
                    assert_eq!(status, TaskRunStatus::Completed);
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::UpdateTaskRunStatus(
                            Ok(()),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        services
            .update_task_run_status("run-update-rt".into(), TaskRunStatus::Completed)
            .await
            .expect("update_task_run_status ok");

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;

        // ── leg 2: Err — server returns Err(String), client surfaces it ──
        let (client2, server2) = UnixStream::pair().expect("pair");
        let server_task2 = tokio::spawn(async move {
            let (mut read, mut write) = server2.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::UpdateTaskRunStatus { run_id, status }) => {
                    assert_eq!(run_id, "run-update-err");
                    assert_eq!(status, TaskRunStatus::Failed);
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::UpdateTaskRunStatus(
                            Err("no such run".into()),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel2 = CancellationToken::new();
        let (services2, bg2) = RpcServices::from_unix_stream(client2, cancel2.clone());
        let err = services2
            .update_task_run_status("run-update-err".into(), TaskRunStatus::Failed)
            .await
            .expect_err("Err leg");
        assert_eq!(err, "no such run");

        drop(services2);
        cancel2.cancel();
        let _ = bg2.reader.await;
        let _ = bg2.writer.await;
        let _ = server_task2.await;
    }

    /// Round-trip a `get_model_context_window` RPC through an in-memory
    /// Unix socket pair.  Exercises both the Ok and Err reply legs.
    #[tokio::test]
    async fn get_model_context_window_roundtrip_over_unixpair() {
        // ── leg 1: Ok ───────────────────────────────────────────────────
        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::GetModelContextWindow { model_id }) => {
                    assert_eq!(model_id, "anthropic/claude-opus-4-7");
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::GetModelContextWindow(
                            Ok(200_000),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let got = services
            .get_model_context_window("anthropic/claude-opus-4-7".into())
            .await
            .expect("get_model_context_window ok");
        assert_eq!(got, 200_000);

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;

        // ── leg 2: Err — surfaces "model not found" ─────────────────────
        let (client2, server2) = UnixStream::pair().expect("pair");
        let server_task2 = tokio::spawn(async move {
            let (mut read, mut write) = server2.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::GetModelContextWindow { .. }) => {
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::GetModelContextWindow(
                            Err("model not found".into()),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel2 = CancellationToken::new();
        let (services2, bg2) = RpcServices::from_unix_stream(client2, cancel2.clone());
        let err = services2
            .get_model_context_window("missing/model".into())
            .await
            .expect_err("Err leg");
        assert_eq!(err, "model not found");

        drop(services2);
        cancel2.cancel();
        let _ = bg2.reader.await;
        let _ = bg2.writer.await;
        let _ = server_task2.await;
    }

    /// Round-trip a `get_provider_base_url` RPC through an in-memory Unix
    /// socket pair.  Exercises both the Ok and Err reply legs.
    #[tokio::test]
    async fn get_provider_base_url_roundtrip_over_unixpair() {
        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::GetProviderBaseUrl {
                    catalog_provider_id,
                }) => {
                    assert_eq!(catalog_provider_id, "anthropic");
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::GetProviderBaseUrl(
                            Ok("https://api.anthropic.com".into()),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let got = services
            .get_provider_base_url("anthropic".into())
            .await
            .expect("get_provider_base_url ok");
        assert_eq!(got, "https://api.anthropic.com");

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;

        // ── Err leg ─────────────────────────────────────────────────────
        let (client2, server2) = UnixStream::pair().expect("pair");
        let server_task2 = tokio::spawn(async move {
            let (mut read, mut write) = server2.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::GetProviderBaseUrl { .. }) => {
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::GetProviderBaseUrl(
                            Err("provider not found".into()),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel2 = CancellationToken::new();
        let (services2, bg2) = RpcServices::from_unix_stream(client2, cancel2.clone());
        let err = services2
            .get_provider_base_url("no-such-provider".into())
            .await
            .expect_err("Err leg");
        assert_eq!(err, "provider not found");

        drop(services2);
        cancel2.cancel();
        let _ = bg2.reader.await;
        let _ = bg2.writer.await;
        let _ = server_task2.await;
    }

    /// Round-trip a `pick_any_default_model` RPC through an in-memory Unix
    /// socket pair.  Exercises both the Some and None reply legs.
    #[tokio::test]
    async fn pick_any_default_model_roundtrip_over_unixpair() {
        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::PickAnyDefaultModel) => {
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::PickAnyDefaultModel(
                            Ok(Some("openai/gpt-4o-mini".into())),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let got = services
            .pick_any_default_model()
            .await
            .expect("pick_any_default_model ok");
        assert_eq!(got.as_deref(), Some("openai/gpt-4o-mini"));

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;

        // ── None leg ────────────────────────────────────────────────────
        let (client2, server2) = UnixStream::pair().expect("pair");
        let server_task2 = tokio::spawn(async move {
            let (mut read, mut write) = server2.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::PickAnyDefaultModel) => {
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::PickAnyDefaultModel(
                            Ok(None),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel2 = CancellationToken::new();
        let (services2, bg2) = RpcServices::from_unix_stream(client2, cancel2.clone());
        let got = services2
            .pick_any_default_model()
            .await
            .expect("pick_any_default_model ok");
        assert!(got.is_none());

        drop(services2);
        cancel2.cancel();
        let _ = bg2.reader.await;
        let _ = bg2.writer.await;
        let _ = server_task2.await;
    }

    /// Round-trip a `create_session` RPC through an in-memory Unix pair.
    #[tokio::test]
    async fn create_session_roundtrip_over_unixpair() {
        use crate::services::SerializableCreateSessionParams;
        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::CreateSession { params }) => {
                    assert_eq!(params.project_id, "p1");
                    let rec = djinn_core::models::SessionRecord {
                        id: "s1".into(),
                        project_id: Some(params.project_id.clone()),
                        task_id: params.task_id.clone(),
                        model_id: params.model.clone(),
                        agent_type: params.agent_type.clone(),
                        started_at: "now".into(),
                        ended_at: None,
                        status: "running".into(),
                        tokens_in: 0,
                        tokens_out: 0,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        task_run_id: params.task_run_id.clone(),
                        title: None,
                        parked_reason: None,
                        cost_usd: None,
                        input_price_per_million_snapshot: None,
                        output_price_per_million_snapshot: None,
                        cache_read_price_per_million_snapshot: None,
                        cache_write_price_per_million_snapshot: None,
                        cost_basis: "unpriced".into(),
                        billing_source: None,
                    };
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::CreateSession(Ok(rec))),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let params = SerializableCreateSessionParams {
            project_id: "p1".into(),
            task_id: Some("t1".into()),
            model: "anthropic/claude-opus-4-7".into(),
            agent_type: "planner".into(),
            metadata_json: None,
            task_run_id: Some("run-1".into()),
            cost_basis_hint: None,
            billing_source: None,
        };
        let got = services
            .create_session(params)
            .await
            .expect("create_session ok");
        assert_eq!(got.id, "s1");
        assert_eq!(got.task_run_id.as_deref(), Some("run-1"));

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;
    }

    /// Round-trip a `publish_session_message` RPC through an in-memory Unix pair.
    #[tokio::test]
    async fn publish_session_message_roundtrip_over_unixpair() {
        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::PublishSessionMessage {
                    session_id,
                    task_id,
                    agent_type,
                    message,
                }) => {
                    assert_eq!(session_id, "s1");
                    assert_eq!(task_id, "t1");
                    assert_eq!(agent_type, "worker");
                    // `message` is opaque JSON over the wire — re-parse
                    // before asserting the field shape.
                    let parsed: serde_json::Value =
                        serde_json::from_str(&message).expect("valid JSON");
                    assert_eq!(parsed["role"], "assistant");
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::PublishSessionMessage(
                            Ok(()),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        services
            .publish_session_message(
                "s1".into(),
                "t1".into(),
                "worker".into(),
                serde_json::json!({"role": "assistant", "content": "hi"}),
            )
            .await
            .expect("publish_session_message ok");

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;
    }

    /// Round-trip an `invoke_llm` RPC through an in-memory Unix pair.
    #[tokio::test]
    async fn invoke_llm_roundtrip_over_unixpair() {
        use djinn_core::message::ContentBlock;
        use djinn_provider::message::{Conversation, Message};
        use djinn_provider::provider::{LlmResponse, TokenUsage, ToolChoice};

        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::InvokeLlm {
                    model_id,
                    conversation,
                    tools,
                    tool_choice,
                }) => {
                    assert_eq!(model_id, "anthropic/claude-opus-4-7");
                    // `conversation` / `tools` are opaque JSON over the
                    // wire — parse before asserting structural shape.
                    let conv_back: Conversation =
                        serde_json::from_str(&conversation).expect("conversation JSON");
                    assert_eq!(conv_back.len(), 1);
                    let tools_back: Vec<serde_json::Value> =
                        serde_json::from_str(&tools).expect("tools JSON");
                    assert_eq!(tools_back.len(), 0);
                    assert_eq!(tool_choice, Some(ToolChoice::Auto));
                    let resp = LlmResponse {
                        content: vec![ContentBlock::text("pong")],
                        thinking: String::new(),
                        usage: TokenUsage {
                            input: 5,
                            output: 4,
                            ..Default::default()
                        },
                    };
                    let payload_str = serde_json::to_string(&resp).expect("encode resp");
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::InvokeLlm(Ok(
                            payload_str,
                        ))),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let mut conv = Conversation::new();
        conv.push(Message::user("ping"));
        let got = services
            .invoke_llm(
                "anthropic/claude-opus-4-7".into(),
                conv,
                vec![],
                Some(ToolChoice::Auto),
            )
            .await
            .expect("invoke_llm ok");
        assert_eq!(got.usage.input, 5);
        assert_eq!(got.usage.output, 4);
        assert_eq!(got.content.len(), 1);

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
        cancel.cancel();
        let _ = bg.reader.await;
        let _ = bg.writer.await;
        let _ = server_task.await;
    }

    /// Round-trip a `get_environment_config` RPC through an in-memory Unix pair.
    #[tokio::test]
    async fn get_environment_config_roundtrip_over_unixpair() {
        let (client, server) = UnixStream::pair().expect("pair");
        let server_task = tokio::spawn(async move {
            let (mut read, mut write) = server.into_split();
            let frame: Frame = read_frame(&mut read).await.expect("read request");
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::GetEnvironmentConfig { project_id }) => {
                    assert_eq!(project_id, "p1");
                    let cfg = djinn_stack::environment::EnvironmentConfig::empty();
                    let cfg_json = serde_json::to_string(&cfg).expect("encode cfg");
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::GetEnvironmentConfig(
                            Ok(cfg_json),
                        )),
                    };
                    write_frame(&mut write, &reply).await.expect("write reply");
                }
                other => panic!("unexpected: {other:?}"),
            }
        });

        let cancel = CancellationToken::new();
        let (services, bg) = RpcServices::from_unix_stream(client, cancel.clone());
        let cfg = services
            .get_environment_config("p1".into())
            .await
            .expect("get_environment_config ok");
        // `EnvironmentConfig::empty()` sets `schema_version = SCHEMA_VERSION`
        // (1). The opaque-JSON wire shape preserves this; the older raw-bincode
        // path silently lost the field, which is why this asserted 0 before.
        assert_eq!(cfg.schema_version, djinn_stack::environment::SCHEMA_VERSION);

        // Drop the `Arc<RpcServices>` (and its inner `mpsc::Sender<Frame>`)
        // BEFORE awaiting the writer — otherwise the writer's `rx.recv()`
        // never returns `None` and the test hangs.  Same drop-ordering
        // bug Phase 10 fixed in `djinn-agent-worker/src/main.rs`.
        drop(services);
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
            total_reopen_count: 0,
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
            created_by_user_id: None,
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
        }
    }
}
