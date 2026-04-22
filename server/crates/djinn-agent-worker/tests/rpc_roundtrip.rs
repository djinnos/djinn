//! End-to-end integration tests for the real RPC wire.
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`
//! swapped the worker transport to TCP + bearer-token handshake.  Phase 2.1
//! additionally moved the terminal `TaskRunReport` off of stdout and onto the
//! shared RPC channel as a `WorkerEvent::TerminalReport` frame — these tests
//! cover both layers end-to-end.
//!
//! Test matrix:
//!
//! - [`worker_roundtrips_load_task_over_tcp`] — happy path.  An in-process
//!   `serve_on_tcp` stands in for djinn-server, an `ExpectedTokenValidator`
//!   accepts the worker's `(task_run_id, token)` pair, the worker dials,
//!   performs the handshake, round-trips `load_task`, emits the terminal
//!   `WorkerEvent::TerminalReport` frame upstream, and exits zero.
//! - [`worker_exits_nonzero_when_tcp_auth_rejects`] — failure path.  The
//!   validator is pinned to a different token; the server answers
//!   `AuthResult { accepted: false, .. }` and closes the socket; the
//!   worker propagates the rejection as a non-zero exit.
//!
//! The server-level unit tests in `djinn_supervisor::services::server::tests`
//! already cover the byte-level handshake semantics against raw
//! `TcpStream`s; these integration tests exist to verify that the
//! `djinn-agent-worker` binary's env-plumbing + `RpcServices::connect_tcp`
//! glue matches what the Pod manifest projects.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use djinn_core::models::{Task, TaskRunTrigger};
use djinn_runtime::{SupervisorFlow, TaskRunSpec, WorkerEvent};
use djinn_supervisor::{
    ExpectedTokenValidator, Frame, FramePayload, RoleKind, ServeHandle, StageError, StageOutcome,
    SupervisorServices, TaskRunOutcome, serve_on_tcp,
};
use djinn_workspace::Workspace;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ── Fixtures ────────────────────────────────────────────────────────────────

fn fixture_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        project_id: "p1".into(),
        short_id: "T-1".into(),
        epic_id: None,
        title: format!("round-tripped:{id}"),
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

fn fixture_spec(task_id: &str) -> TaskRunSpec {
    TaskRunSpec {
        task_id: task_id.into(),
        project_id: "proj-xyz".into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: format!("djinn/{task_id}"),
        flow: SupervisorFlow::Planning,
        model_id_per_role: HashMap::new(),
    }
}

/// Minimal host-side [`SupervisorServices`] that returns a canned task on
/// `load_task` and panics on the other trait methods — the placeholder
/// driver in `djinn-agent-worker` only exercises `load_task` today.
///
/// Duplicated across this file + `server.rs` unit tests (both under
/// `#[cfg(test)]`); shared extraction can wait until more integration
/// tests need it.
struct FakeServices {
    cancel: CancellationToken,
    canned_task_id: String,
}

#[async_trait::async_trait]
impl SupervisorServices for FakeServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn load_task(&self, task_id: String) -> Result<Task, String> {
        assert_eq!(
            task_id, self.canned_task_id,
            "worker asked for an unexpected task_id"
        );
        Ok(fixture_task(&task_id))
    }

    async fn execute_stage(
        &self,
        _task: &Task,
        _workspace: &Workspace,
        _role_kind: RoleKind,
        _task_run_id: &str,
        _spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        unimplemented!("not exercised in PR 2 integration tests")
    }

    async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
        unimplemented!("not exercised in PR 2 integration tests")
    }
}

/// Bind `serve_on_tcp` to `127.0.0.1:0`, return the resolved `(SocketAddr,
/// ServeHandle)` pair.  Tests don't care which port the kernel picked.
async fn start_server(
    services: Arc<dyn SupervisorServices>,
    validator: Arc<ExpectedTokenValidator>,
) -> (SocketAddr, ServeHandle) {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let handle = serve_on_tcp(addr, services, validator, None)
        .await
        .expect("bind serve_on_tcp");
    let bound = handle.bound_addr.expect("bound addr");
    (bound, handle)
}

/// Slot the raw TCP stand-in server drops captured `WorkerEvent::TerminalReport`
/// frames into so the happy-path test can assert the worker emitted one.
///
/// The `serve_on_tcp` reader loop drops `FramePayload::Event` frames today;
/// Phase C threads a [`djinn_supervisor::ConnectionRegistry`] through the
/// accept loop to route them into per-task-run event channels.  Until that
/// lands, this test spins up its own raw TCP listener (bypassing
/// `serve_on_tcp`) to drive the handshake + `load_task` round-trip + capture
/// the terminal event.
#[derive(Default)]
struct CapturedEvents {
    terminal: Mutex<Option<djinn_runtime::TaskRunReport>>,
}

impl CapturedEvents {
    async fn record_terminal(&self, report: djinn_runtime::TaskRunReport) {
        *self.terminal.lock().await = Some(report);
    }

    async fn get_terminal(&self) -> Option<djinn_runtime::TaskRunReport> {
        self.terminal.lock().await.clone()
    }
}

/// Spin up a raw `tokio::net::TcpListener`-backed djinn-server stand-in:
///
/// 1. Accepts one worker connection.
/// 2. Reads the AuthHello, replies with `AuthResult { accepted: true }`.
/// 3. Dispatches `LoadTask` RPCs through a canned `FakeServices`-like
///    closure.
/// 4. Captures the first `WorkerEvent::TerminalReport` event it sees in the
///    supplied [`CapturedEvents`] slot and returns when the stream closes.
///
/// Returns `(SocketAddr, JoinHandle<()>)` — drop the handle (or let the
/// connection close naturally) to stop the server.
async fn start_capturing_server(
    canned_task_id: String,
    expected_token: String,
    expected_task_run_id: String,
    captured: Arc<CapturedEvents>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use djinn_runtime::wire::{read_frame, write_frame};
    use djinn_supervisor::{AuthHelloMsg, AuthResultMsg, ServiceRpcRequest, ServiceRpcResponse};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw listener");
    let addr = listener.local_addr().expect("local_addr");

    let handle = tokio::spawn(async move {
        let (mut stream, _peer) = listener.accept().await.expect("accept worker");

        // 1. Handshake: read AuthHello.
        let hello: Frame = read_frame(&mut stream).await.expect("read AuthHello");
        let hello_correlation = hello.correlation_id;
        let (task_run_id, token) = match hello.payload {
            FramePayload::AuthHello(AuthHelloMsg { task_run_id, token }) => (task_run_id, token),
            other => panic!("expected AuthHello, got {other:?}"),
        };
        assert_eq!(token, expected_token, "handshake token mismatch");
        assert_eq!(
            task_run_id, expected_task_run_id,
            "handshake task_run_id mismatch"
        );
        let ack = Frame {
            correlation_id: hello_correlation,
            payload: FramePayload::AuthResult(AuthResultMsg {
                accepted: true,
                error: None,
            }),
        };
        write_frame(&mut stream, &ack).await.expect("write ack");

        // 2. Dispatch loop: handle LoadTask RPCs, capture TerminalReport
        //    events, return when the stream closes.
        loop {
            let frame: Frame = match read_frame(&mut stream).await {
                Ok(f) => f,
                Err(_) => return,
            };
            match frame.payload {
                FramePayload::Rpc(ServiceRpcRequest::LoadTask { task_id }) => {
                    assert_eq!(task_id, canned_task_id, "unexpected LoadTask id");
                    let mut t = fixture_task(&task_id);
                    t.title = format!("loaded:{task_id}");
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(ServiceRpcResponse::LoadTask(Ok(t))),
                    };
                    write_frame(&mut stream, &reply).await.expect("write reply");
                }
                FramePayload::Event(WorkerEvent::TerminalReport(report)) => {
                    captured.record_terminal(report).await;
                }
                FramePayload::Event(other) => {
                    tracing::debug!(?other, "ignored non-terminal event");
                }
                other => {
                    panic!("unexpected frame on dispatch loop: {other:?}");
                }
            }
        }
    });

    (addr, handle)
}

/// Serialize `spec` to a tempfile, return the file's path so the worker
/// can be pointed at it via `DJINN_SPEC_PATH`.
fn write_spec(dir: &std::path::Path, spec: &TaskRunSpec) -> std::path::PathBuf {
    let path = dir.join("spec.bin");
    let bytes = bincode::serialize(spec).expect("serialize spec");
    std::fs::write(&path, bytes).expect("write spec file");
    path
}

/// Drop a canned token file and return its path — the worker reads this as
/// its projected ServiceAccount token.
fn write_token(dir: &std::path::Path, token: &str) -> std::path::PathBuf {
    let path = dir.join("token");
    std::fs::write(&path, token).expect("write token file");
    path
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Happy-path TCP round-trip: validator accepts the handshake, worker
/// round-trips `load_task`, emits a `WorkerEvent::TerminalReport` upstream,
/// and exits zero.  Uses a raw `TcpListener`-backed stand-in server because
/// `serve_on_tcp`'s reader loop drops `Event` frames today — Phase C
/// (`ConnectionRegistry`) is where event routing lands on the real server
/// path.
#[tokio::test]
async fn worker_roundtrips_load_task_over_tcp() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let task_run_id = "run-tcp-1";
    let bearer = "kubeSA-projected-token-abc";
    let task_id = "task-abc";

    // 1. Stand up a raw-TCP djinn-server stand-in that drives the handshake,
    //    answers `LoadTask`, and captures the worker's TerminalReport event.
    let captured = Arc::new(CapturedEvents::default());
    let (addr, server) = start_capturing_server(
        task_id.into(),
        bearer.into(),
        task_run_id.into(),
        captured.clone(),
    )
    .await;

    // 2. Materialise the spec + token files the worker reads at boot.
    let spec = fixture_spec(task_id);
    let spec_path = write_spec(tempdir.path(), &spec);
    let token_path = write_token(tempdir.path(), bearer);

    // 3. Spawn the worker binary with env-only config — mirroring the Pod
    //    manifest shape the `KubernetesRuntime` will produce.
    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let child = tokio::process::Command::new(exe)
        .arg("task-run")
        .env("DJINN_SERVER_ADDR", addr.to_string())
        .env("DJINN_SPEC_PATH", &spec_path)
        .env("DJINN_TOKEN_PATH", &token_path)
        .env("DJINN_TASK_RUN_ID", task_run_id)
        .env("DJINN_WORKSPACE_PATH", tempdir.path())
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    // 4. Wait for the worker to exit.  Stdout is now reserved for log
    //    routing — the terminal report travels on the RPC channel and is
    //    captured by the stand-in server above.
    let output = child
        .wait_with_output()
        .await
        .expect("wait for worker exit");
    assert!(
        output.status.success(),
        "worker exited non-zero: status={:?} stderr=\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // 5. Let the server task drain (it exits once the worker's stream EOFs)
    //    and assert the TerminalReport event landed.
    //
    //    The worker closes its writer after emitting the TerminalReport,
    //    which EOFs our raw read loop — `server.await` returns once that
    //    happens, so a bounded timeout is a belt-and-braces guard against
    //    a flaky worker exit.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("capturing server should exit within 5s of worker close")
        .expect("capturing server task should not panic");

    let report = captured
        .get_terminal()
        .await
        .expect("worker should have emitted a WorkerEvent::TerminalReport over RPC");
    match report.outcome {
        TaskRunOutcome::Interrupted => {}
        other => panic!("unexpected outcome: {other:?}"),
    }

    // 6. Nothing on stdout now that the report rides the RPC channel.  A
    //    small tolerance for tracing stragglers that sneak onto stdout is
    //    unnecessary — the worker pins tracing to stderr.
    assert!(
        output.stdout.is_empty(),
        "worker stdout should be empty after PR-2.1 — got {} bytes; stderr=\n{}",
        output.stdout.len(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Rejection path: the validator is pinned to a different token, so the
/// server answers `AuthResult { accepted: false, .. }` and closes the
/// connection.  The worker propagates the handshake rejection as a
/// non-zero process exit.
#[tokio::test]
async fn worker_exits_nonzero_when_tcp_auth_rejects() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let task_run_id = "run-tcp-reject";
    let task_id = "task-reject";
    let real_bearer = "correct-token";
    let bogus_bearer = "wrong-token";

    // Validator expects `real_bearer`, but we point the worker at the
    // bogus one — forcing the handshake to fail.
    let services: Arc<dyn SupervisorServices> = Arc::new(FakeServices {
        cancel: CancellationToken::new(),
        canned_task_id: task_id.into(),
    });
    let validator = Arc::new(ExpectedTokenValidator::new(real_bearer, task_run_id));
    let (addr, server) = start_server(services, validator).await;

    let spec = fixture_spec(task_id);
    let spec_path = write_spec(tempdir.path(), &spec);
    let token_path = write_token(tempdir.path(), bogus_bearer);

    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let child = tokio::process::Command::new(exe)
        .arg("task-run")
        .env("DJINN_SERVER_ADDR", addr.to_string())
        .env("DJINN_SPEC_PATH", &spec_path)
        .env("DJINN_TOKEN_PATH", &token_path)
        .env("DJINN_TASK_RUN_ID", task_run_id)
        .env("DJINN_WORKSPACE_PATH", tempdir.path())
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    let output = child
        .wait_with_output()
        .await
        .expect("wait for worker exit");
    assert!(
        !output.status.success(),
        "worker unexpectedly exited zero after auth rejection; stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The failure reason should mention the auth rejection somewhere on
    // stderr — loose match keeps the test tolerant of log-level tweaks.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("auth") || stderr.contains("reject") || stderr.contains("Auth"),
        "expected stderr to mention auth/reject; got:\n{stderr}"
    );

    server.cancel();
    let _ = server.join.await;
}
