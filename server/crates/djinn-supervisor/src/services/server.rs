//! Launcher-side bincode-RPC server for [`SupervisorServices`].
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//!
//! [`serve_on_tcp`] is the production transport — worker Pods dial the
//! djinn-server ClusterIP Service and present an [`AuthHello`] handshake
//! validated through an injected [`TokenValidator`].  The older
//! [`serve_on_unix_socket`] stays alongside it for in-process tests that
//! exercise the shared dispatch loop without the auth round-trip.
//!
//! ## Transports
//!
//! | Transport | Handshake | Intended caller |
//! |---|---|---|
//! | `serve_on_unix_socket` | none — filesystem perms | in-process tests |
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
use djinn_runtime::{StreamEvent, TaskRunReport, WorkerEvent};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{Mutex, mpsc, watch};
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
/// - `djinn_agent::runtime_bridge::K8sTokenReviewValidator` — posts a
///   `TokenReview` against the in-cluster apiserver via
///   `djinn_k8s::token_review::review_token` and returns the authenticated
///   SA identity.  Wired into djinn-server boot in
///   `server::state::AppState::start_rpc_listener_if_needed`.
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
    /// [`ConnectionRegistry`] the TCP accept loop routes event frames
    /// through, when one was threaded into [`serve_on_tcp`].  `None` on the
    /// unix-socket test path.
    registry: Option<Arc<ConnectionRegistry>>,
}

impl ServeHandle {
    /// Signal the server to stop.
    pub fn cancel(&self) {
        self.cancel_server.cancel();
    }

    /// Read-only access to the [`ConnectionRegistry`] the server is routing
    /// through, when one is present.  Runtimes ([`djinn_k8s::KubernetesRuntime`])
    /// hold onto this to call [`ConnectionRegistry::register_pending`] before
    /// dialling a worker.
    pub fn registry(&self) -> Option<Arc<ConnectionRegistry>> {
        self.registry.clone()
    }
}

// ── ConnectionRegistry ───────────────────────────────────────────────────────

/// Per-task-run connection table that bridges the host's `SessionRuntime`
/// impls and the [`serve_on_tcp`] accept loop.
///
/// The registry solves two Phase 2.1 problems:
///
/// 1. **Inbound event routing** — when a worker Pod dials the launcher and
///    completes the `AuthHello` handshake, the accept loop looks up the
///    pending connection by `task_run_id` and forwards every subsequent
///    `FramePayload::Event` onto the registered [`mpsc::Sender<StreamEvent>`].
///    The runtime drains the matching receiver in its `teardown` path to
///    pick up the real `WorkerEvent::TerminalReport` instead of synthesising
///    one from Job status.
/// 2. **Outbound control frames** — the accept loop publishes a per-connection
///    [`mpsc::Sender<Frame>`] back into the pending slot so the runtime can
///    send `Control(Cancel)` / `Control(Shutdown)` at a specific worker
///    during `cancel()`.
///
/// Reservation and attachment are two phases so the runtime can reserve the
/// slot *before* it creates the K8s Job (avoiding a race where the Pod
/// connects back faster than `runtime.prepare` returns).  [`PendingConnection::
/// wait_for_connection`] blocks until `attach` flips the watch channel true,
/// so `attach_stdio` can await connectivity without polling.
///
/// `ConnectionRegistry` is cheap to clone (the inner state sits behind a
/// `Mutex`) and designed to be held in an `Arc` on both sides.
#[derive(Debug, Default)]
pub struct ConnectionRegistry {
    inner: Mutex<HashMap<String, ConnSlot>>,
}

/// Internal state for one registered task-run connection.
#[derive(Debug)]
struct ConnSlot {
    /// Channel the accept loop forwards inbound `FramePayload::Event` frames
    /// into after the handshake succeeds.
    events_tx: mpsc::Sender<StreamEvent>,
    /// Outbound frame sender the accept loop publishes on successful attach.
    /// Runtimes route `FramePayload::Control(_)` through this.
    /// `None` until the worker completes the handshake.
    outbound_tx: Option<mpsc::Sender<Frame>>,
    /// Flipped `true` when the handshake succeeds.  `PendingConnection::
    /// wait_for_connection` awaits this watch channel.
    connected_tx: watch::Sender<bool>,
    /// Username the validator returned at handshake time — kept for audit
    /// logs.
    #[allow(dead_code)]
    username: Option<String>,
}

/// Handle returned by [`ConnectionRegistry::register_pending`].
///
/// Carries the channels the runtime drains / pushes on during the task-run
/// lifecycle:
///
/// - [`PendingConnection::events_rx`] — inbound `WorkerEvent` stream.  The
///   runtime drains this until it observes the terminal report (or the
///   sender closes).
/// - [`PendingConnection::wait_for_connection`] — resolves `Ok(())` once the
///   worker's `AuthHello` is accepted and the outbound sender is populated.
/// - [`PendingConnection::outbound_sender`] — grabs the outbound sender so
///   the runtime can push `Control` frames.  Returns `None` until the
///   handshake completes.
pub struct PendingConnection {
    task_run_id: String,
    registry: Arc<ConnectionRegistry>,
    pub events_rx: mpsc::Receiver<StreamEvent>,
    connected_rx: watch::Receiver<bool>,
}

impl PendingConnection {
    /// The task-run id this pending connection is bound to.
    pub fn task_run_id(&self) -> &str {
        &self.task_run_id
    }

    /// Wait until the worker has completed the handshake.  Returns `Ok(())`
    /// immediately if the connection is already live; otherwise awaits the
    /// [`watch::Receiver`] transition to `true`.
    ///
    /// Returns `Err(String)` only when the watch channel is closed before
    /// the connection goes live (the registry was deregistered without a
    /// successful attach, e.g. the Pod failed before dialling).
    pub async fn wait_for_connection(&mut self) -> Result<(), String> {
        if *self.connected_rx.borrow() {
            return Ok(());
        }
        loop {
            self.connected_rx
                .changed()
                .await
                .map_err(|e| format!("pending connection watch closed: {e}"))?;
            if *self.connected_rx.borrow() {
                return Ok(());
            }
        }
    }

    /// Grab a clone of the outbound [`Frame`] sender the accept loop
    /// published on handshake acceptance.  Returns `None` until
    /// [`Self::wait_for_connection`] has resolved `Ok(())`.
    pub async fn outbound_sender(&self) -> Option<mpsc::Sender<Frame>> {
        self.registry.outbound_sender_for(&self.task_run_id).await
    }
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        // Best-effort: if the runtime dropped us without going through
        // `teardown`, clean the registry slot so a later `register_pending`
        // with the same id doesn't collide.  Done on a detached task because
        // `Drop` isn't async.
        let registry = self.registry.clone();
        let task_run_id = self.task_run_id.clone();
        tokio::spawn(async move {
            registry.deregister(&task_run_id).await;
        });
    }
}

impl ConnectionRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve a slot for the given `task_run_id` before the runtime
    /// actually dials the worker.
    ///
    /// Returns a [`PendingConnection`] carrying the channels the runtime
    /// will drain / push on during the task-run lifecycle.  The `buffer`
    /// bounds both the inbound `StreamEvent` channel — if the runtime
    /// stalls on `events_rx` the accept loop back-pressures the worker.
    ///
    /// Errors if a slot for the same `task_run_id` is already registered.
    pub async fn register_pending(
        self: &Arc<Self>,
        task_run_id: impl Into<String>,
        buffer: usize,
    ) -> Result<PendingConnection, String> {
        let task_run_id = task_run_id.into();
        let (events_tx, events_rx) = mpsc::channel::<StreamEvent>(buffer);
        let (connected_tx, connected_rx) = watch::channel(false);

        let mut map = self.inner.lock().await;
        if map.contains_key(&task_run_id) {
            return Err(format!(
                "task_run_id {task_run_id} already has a pending connection"
            ));
        }
        map.insert(
            task_run_id.clone(),
            ConnSlot {
                events_tx,
                outbound_tx: None,
                connected_tx,
                username: None,
            },
        );
        drop(map);

        Ok(PendingConnection {
            task_run_id,
            registry: Arc::clone(self),
            events_rx,
            connected_rx,
        })
    }

    /// Called by the TCP accept loop immediately after a successful
    /// `AuthHello`.  Publishes the outbound frame sender into the pending
    /// slot (if one was pre-registered) and flips the `connected` watch to
    /// `true` so [`PendingConnection::wait_for_connection`] resolves.
    ///
    /// Returns an [`AttachedSlot`] carrying the `events_tx` the accept
    /// loop should forward `FramePayload::Event` frames into.  Returns
    /// `None` when no pending registration exists — the accept loop falls
    /// back to the old generic-dispatch path (preserves back-compat for
    /// unit tests that don't pre-register).
    async fn attach(
        &self,
        task_run_id: &str,
        outbound_tx: mpsc::Sender<Frame>,
        username: Option<String>,
    ) -> Option<AttachedSlot> {
        let mut map = self.inner.lock().await;
        let slot = map.get_mut(task_run_id)?;
        slot.outbound_tx = Some(outbound_tx);
        slot.username = username;
        // watch::Sender::send ignores receivers; it only errors if all
        // receivers dropped, in which case the runtime already gave up on
        // the connection.  Either way, ignore send failures.
        let _ = slot.connected_tx.send(true);
        Some(AttachedSlot {
            events_tx: slot.events_tx.clone(),
        })
    }

    /// Grab the outbound sender for a task-run, when a pending connection
    /// has been attached.
    ///
    /// Exposed so runtimes ([`djinn_k8s::KubernetesRuntime`]) can push
    /// `FramePayload::Control(_)` at a specific worker during `cancel()`
    /// without having to retain the `PendingConnection` handle.
    pub async fn outbound_sender_for(&self, task_run_id: &str) -> Option<mpsc::Sender<Frame>> {
        let map = self.inner.lock().await;
        map.get(task_run_id)
            .and_then(|slot| slot.outbound_tx.clone())
    }

    /// Remove the slot for the given task-run.  Idempotent.
    pub async fn deregister(&self, task_run_id: &str) {
        let mut map = self.inner.lock().await;
        map.remove(task_run_id);
    }
}

/// Per-connection handle the accept loop uses after a successful attach.
struct AttachedSlot {
    events_tx: mpsc::Sender<StreamEvent>,
}

/// Lower a `WorkerEvent` frame into a [`StreamEvent`] the registry speaks.
///
/// Phase 2.1 only delivers [`WorkerEvent::TerminalReport`] upstream —
/// everything else is either legacy (`Placeholder`) or not yet wired.  When
/// new variants land, route them to the matching `StreamEvent` variants
/// here (assistant deltas, tool calls, stage outcomes, heartbeats).
fn worker_event_to_stream_event(event: WorkerEvent) -> Option<StreamEvent> {
    match event {
        WorkerEvent::TerminalReport(report) => Some(StreamEvent::Report(report)),
        WorkerEvent::Placeholder => None,
    }
}

/// Compile-time check that [`TaskRunReport`] is importable from this module
/// (so a future refactor that hides it breaks here before it breaks at the
/// K8s runtime).
#[allow(dead_code)]
fn _taskrunreport_is_in_scope(_: &TaskRunReport) {}

// ── serve_on_unix_socket (in-process test path) ──────────────────────────────

/// Bind a Unix-domain socket at `path` and spawn the accept loop.
///
/// Intended for in-process tests only.  The TCP path ([`serve_on_tcp`]) is
/// the production transport after Phase 2 K8s PR 2; this entry point stays
/// functional so `rpc_roundtrip.rs` keeps exercising the dispatch without
/// extra auth machinery.
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
                        let (reply_tx, reply_rx) = mpsc::channel::<Frame>(64);
                        dispatch_loop(
                            read_half,
                            write_half,
                            services_arc,
                            reply_tx,
                            reply_rx,
                            None, // unix path has no registry — events dropped as before
                            cancel_child.clone(),
                        )
                        .await;
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
        registry: None,
    })
}

// ── serve_on_tcp (PR 2) ──────────────────────────────────────────────────────

/// Per-task-run connection state stored on the listener.
///
/// Single entry per `task_run_id`; the listener rejects a second AuthHello
/// bearing an already-bound task_run_id.  The value is intentionally
/// minimal — richer per-connection state lives in [`ConnectionRegistry`],
/// which is the host-facing bridge.
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
///
/// `registry` — when `Some`, the accept loop looks up a pre-registered
/// [`PendingConnection`] by `task_run_id` on handshake acceptance and
/// forwards every subsequent `FramePayload::Event` frame into the pending
/// connection's `events_tx`.  When `None`, the accept loop falls back to
/// the old generic-dispatch path (Event frames are logged + dropped).
/// This is the Phase 2.1 bridge the `KubernetesRuntime` uses to pick up
/// real terminal reports instead of synthesising them from Job status.
pub async fn serve_on_tcp<V: TokenValidator>(
    addr: SocketAddr,
    services: Arc<dyn SupervisorServices>,
    validator: Arc<V>,
    registry: Option<Arc<ConnectionRegistry>>,
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
    let registry_for_handle = registry.clone();

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
                            let registry = registry.clone();
                            let cancel_conn = cancel_child.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_tcp_connection(
                                    stream,
                                    services,
                                    validator,
                                    conns,
                                    registry,
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
        registry: registry_for_handle,
    })
}

/// Run one TCP connection: read the AuthHello, validate, register, then
/// enter the shared dispatch loop.
async fn handle_tcp_connection<V: TokenValidator>(
    stream: tokio::net::TcpStream,
    services: Arc<dyn SupervisorServices>,
    validator: Arc<V>,
    conns: Arc<Mutex<HashMap<String, ConnState>>>,
    registry: Option<Arc<ConnectionRegistry>>,
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

    // 5. Pre-allocate the outbound reply channel so the ConnectionRegistry
    //    can expose it to the runtime — this is what lets the runtime push
    //    `FramePayload::Control(_)` at a specific worker during `cancel()`.
    let (reply_tx, reply_rx) = mpsc::channel::<Frame>(64);

    // 6. If a registry was threaded in and a pending connection is waiting
    //    for this `task_run_id`, attach — publishes the outbound sender and
    //    flips `PendingConnection::wait_for_connection` to ready.  The
    //    returned [`AttachedSlot::events_tx`] is the channel inbound Event
    //    frames should land on.
    let events_tx = if let Some(registry) = registry.as_ref() {
        registry
            .attach(
                &task_run_id,
                reply_tx.clone(),
                validation.username.clone(),
            )
            .await
            .map(|slot| slot.events_tx)
    } else {
        None
    };

    // 7. Enter the shared dispatch loop.
    dispatch_loop(read_half, write_half, services, reply_tx, reply_rx, events_tx, cancel).await;

    // 8. Deregister.
    conns.lock().await.remove(&task_run_id);
    if let Some(registry) = registry.as_ref() {
        registry.deregister(&task_run_id).await;
    }
    Ok(())
}

// ── Shared dispatch loop ─────────────────────────────────────────────────────

/// Drive the read/write pair for one post-handshake connection.
///
/// Parameterised on any split `AsyncRead` + `AsyncWrite` so it works over
/// both the unix-socket halves and the TCP halves without duplicating the
/// reader/writer boilerplate.
///
/// `events_tx` — when `Some`, inbound `FramePayload::Event` frames are
/// lowered to [`StreamEvent`] via [`worker_event_to_stream_event`] and
/// forwarded into this channel.  When `None`, event frames are logged +
/// dropped (preserves the pre-Phase-2.1 behaviour for the unix-socket test
/// path and for TCP callers that don't supply a `ConnectionRegistry`).
async fn dispatch_loop<R, W>(
    read_half: R,
    write_half: W,
    services: Arc<dyn SupervisorServices>,
    reply_tx: mpsc::Sender<Frame>,
    reply_rx: mpsc::Receiver<Frame>,
    events_tx: Option<mpsc::Sender<StreamEvent>>,
    cancel: CancellationToken,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = tokio::spawn(writer_loop(write_half, reply_rx, cancel.clone()));
    reader_loop(read_half, services, reply_tx, events_tx, cancel.clone()).await;

    // Closing `reply_tx` drains the writer naturally.
    let _ = writer.await;
}

async fn reader_loop<R>(
    mut read_half: R,
    services: Arc<dyn SupervisorServices>,
    reply_tx: mpsc::Sender<Frame>,
    events_tx: Option<mpsc::Sender<StreamEvent>>,
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
                            FramePayload::Event(event) => {
                                match (events_tx.as_ref(), worker_event_to_stream_event(event)) {
                                    (Some(tx), Some(stream_event)) => {
                                        if tx.send(stream_event).await.is_err() {
                                            debug!("server reader: events_tx dropped");
                                        }
                                    }
                                    (Some(_tx), None) => {
                                        debug!(
                                            "server reader: received WorkerEvent with no StreamEvent mapping"
                                        );
                                    }
                                    (None, _) => {
                                        debug!(
                                            "server reader: event frame on connection without registry — dropped"
                                        );
                                    }
                                }
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
        let handle = serve_on_tcp(addr, services, validator, None)
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
        let handle = serve_on_tcp(addr, services, validator, None)
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
        let handle = serve_on_tcp(addr, services, validator, None)
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

    /// Pre-register a pending connection on a `ConnectionRegistry`, dial the
    /// server, complete the handshake, send a `FramePayload::Event(
    /// WorkerEvent::TerminalReport(..))`, and assert the event arrives on
    /// the pending connection's `events_rx` as a `StreamEvent::Report`.
    ///
    /// Also verifies that `PendingConnection::wait_for_connection` resolves
    /// once the handshake succeeds, and that `outbound_sender` yields a
    /// live `mpsc::Sender<Frame>` post-handshake.
    #[tokio::test]
    async fn serve_on_tcp_routes_event_to_pending_connection() {
        use djinn_runtime::{
            RoleKind, StreamEvent, TaskRunOutcome, TaskRunReport, WorkerEvent,
        };

        let services: Arc<dyn SupervisorServices> = Arc::new(FakeServices {
            cancel: CancellationToken::new(),
            canned_task_id: "registry-task-1".into(),
        });
        let validator = Arc::new(AllowAllValidator);
        let registry = Arc::new(ConnectionRegistry::new());
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let handle = serve_on_tcp(addr, services, validator, Some(registry.clone()))
            .await
            .expect("bind tcp with registry");
        let bound = handle.bound_addr.expect("bound addr");

        // Reserve a pending slot BEFORE the worker dials — this is the
        // production sequence (runtime.prepare registers, then creates Job).
        let task_run_id = "registry-run-1".to_string();
        let mut pending = registry
            .register_pending(task_run_id.clone(), 16)
            .await
            .expect("register_pending");

        // Dial and present the AuthHello.
        let mut stream = TcpStream::connect(bound).await.expect("connect");
        let hello = Frame {
            correlation_id: 1,
            payload: FramePayload::AuthHello(AuthHelloMsg {
                task_run_id: task_run_id.clone(),
                token: "any-token".into(),
            }),
        };
        write_frame(&mut stream, &hello).await.expect("write hello");
        let reply: Frame = read_frame(&mut stream).await.expect("read ack");
        match reply.payload {
            FramePayload::AuthResult(AuthResultMsg { accepted: true, .. }) => {}
            other => panic!("expected accepted auth, got {other:?}"),
        }

        // wait_for_connection should now resolve promptly.
        tokio::time::timeout(std::time::Duration::from_secs(2), pending.wait_for_connection())
            .await
            .expect("wait_for_connection should resolve within 2s of handshake")
            .expect("wait_for_connection Ok");

        // Outbound sender is live post-handshake.
        let outbound = pending
            .outbound_sender()
            .await
            .expect("outbound sender should be populated after attach");

        // Send a TerminalReport event from the "worker" side.  This is
        // what `RpcServices::emit_event(WorkerEvent::TerminalReport(..))`
        // would push on the production path.
        let report = TaskRunReport {
            task_run_id: task_run_id.clone(),
            outcome: TaskRunOutcome::Closed {
                reason: "planner-only flow finished".into(),
            },
            stages_completed: vec![RoleKind::Planner],
        };
        let event_frame = Frame {
            correlation_id: 0,
            payload: FramePayload::Event(WorkerEvent::TerminalReport(report.clone())),
        };
        write_frame(&mut stream, &event_frame)
            .await
            .expect("write Event frame");

        // Drain pending.events_rx until we see the StreamEvent::Report.
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match pending.events_rx.recv().await {
                    Some(StreamEvent::Report(r)) => return r,
                    Some(other) => {
                        tracing::debug!(?other, "skipping non-terminal event");
                    }
                    None => panic!("events_rx closed before receiving terminal report"),
                }
            }
        })
        .await
        .expect("terminal report should land on pending.events_rx within 2s");

        assert_eq!(received.task_run_id, report.task_run_id);
        match received.outcome {
            TaskRunOutcome::Closed { reason } => {
                assert_eq!(reason, "planner-only flow finished");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }

        // Sanity: outbound_sender is the same channel the host will use for
        // `Control(Cancel)`.  A round-trip through it would block the
        // writer task (the test server's writer is draining it), so we only
        // assert the channel is not closed.
        assert!(!outbound.is_closed(), "outbound channel should stay open");

        // Teardown.
        drop(pending);
        handle.cancel();
        let _ = handle.join.await;
    }
}
