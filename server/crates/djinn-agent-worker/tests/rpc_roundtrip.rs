//! End-to-end integration tests for the real RPC wire.
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//!
//! The worker binary's transport flipped from AF_UNIX to TCP + bearer-token
//! handshake in PR 2.  These tests spawn the binary the way the
//! `KubernetesRuntime` will (env-driven, spec on a file, token on a file,
//! destination is `host:port`) and drive the wire round-trip end-to-end.
//!
//! Test matrix:
//!
//! - [`worker_roundtrips_load_task_over_tcp`] — happy path.  An in-process
//!   `serve_on_tcp` stands in for djinn-server, an `ExpectedTokenValidator`
//!   accepts the worker's `(task_run_id, token)` pair, the worker dials,
//!   performs the handshake, round-trips `load_task`, and exits zero with
//!   a `TaskRunReport` on stdout.
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

use djinn_core::models::{Task, TaskRunTrigger};
use djinn_runtime::{SupervisorFlow, TaskRunReport, TaskRunSpec};
use djinn_supervisor::{
    ExpectedTokenValidator, RoleKind, ServeHandle, StageError, StageOutcome, SupervisorServices,
    TaskRunOutcome, serve_on_tcp,
};
use djinn_workspace::Workspace;
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
    let handle = serve_on_tcp(addr, services, validator)
        .await
        .expect("bind serve_on_tcp");
    let bound = handle.bound_addr.expect("bound addr");
    (bound, handle)
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
/// round-trips `load_task`, exits zero with a `TaskRunReport` on stdout.
#[tokio::test]
async fn worker_roundtrips_load_task_over_tcp() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let task_run_id = "run-tcp-1";
    let bearer = "kubeSA-projected-token-abc";
    let task_id = "task-abc";

    // 1. Stand up an in-process djinn-server that accepts a fixed (token,
    //    task_run_id) pair.
    let services: Arc<dyn SupervisorServices> = Arc::new(FakeServices {
        cancel: CancellationToken::new(),
        canned_task_id: task_id.into(),
    });
    let validator = Arc::new(ExpectedTokenValidator::new(bearer, task_run_id));
    let (addr, server) = start_server(services, validator).await;

    // 2. Materialise the spec + token files the worker reads at boot.
    let spec = fixture_spec(task_id);
    let spec_path = write_spec(tempdir.path(), &spec);
    let token_path = write_token(tempdir.path(), bearer);

    // 3. Spawn the worker binary with env-only config — mirroring the Pod
    //    manifest shape the `KubernetesRuntime` will produce.
    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let child = tokio::process::Command::new(exe)
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

    // 4. Wait for the worker to exit and collect its stdout frame.
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

    // 5. Decode the terminal TaskRunReport frame off stdout.
    assert!(
        output.stdout.len() >= 4,
        "stdout too short to hold a frame; stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let len = u32::from_be_bytes([
        output.stdout[0],
        output.stdout[1],
        output.stdout[2],
        output.stdout[3],
    ]) as usize;
    assert!(
        output.stdout.len() >= 4 + len,
        "stdout too short for declared frame length {len}"
    );
    let report: TaskRunReport =
        bincode::deserialize(&output.stdout[4..4 + len]).expect("decode TaskRunReport");
    match report.outcome {
        TaskRunOutcome::Interrupted => {}
        other => panic!("unexpected outcome: {other:?}"),
    }

    // 6. Tear the server down.
    server.cancel();
    let _ = server.join.await;
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
