//! End-to-end integration test for the real RPC wire.
//!
//! Phase 2 PR 5 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! The test harness:
//!
//! 1. Binds a Unix-domain socket at `<tempdir>/svc.sock`.
//! 2. Spawns the `djinn-agent-worker` binary with `--ipc-socket` pointing at
//!    that socket and `--workspace-path` pointing at the same tempdir.
//! 3. Pipes a bincode-serialized `TaskRunSpec` into the worker's stdin.
//! 4. Accepts the worker's socket connection, reads the expected `LoadTask`
//!    request frame, replies with a canned `Task`, and asserts the worker
//!    exits cleanly with the terminal `TaskRunReport` on stderr.
//!
//! This is the PR-5 smoke test the blueprint calls out: "new
//! `cargo test -p djinn-agent-worker --test rpc_roundtrip` proves bincode
//! round-trip for ≥ 5 RPC variants".  This file proves the full wire at the
//! binary level; the ≥ 5 variant roundtrip assertions live in unit tests
//! inside `djinn-supervisor::services::wire::tests`.

use std::process::Stdio;

use djinn_core::models::{Task, TaskRunTrigger};
use djinn_runtime::wire::{read_frame, write_frame};
use djinn_runtime::{SupervisorFlow, TaskRunReport, TaskRunSpec};
use djinn_supervisor::services::wire::{
    Frame, FramePayload, ServiceRpcRequest, ServiceRpcResponse,
};
use std::collections::HashMap;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;

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

#[tokio::test]
async fn worker_roundtrips_load_task_over_unix_socket() {
    // 1. Tempdir hosts both the socket and the bind-mounted "workspace"
    //    directory the worker `attach_existing`s to.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let socket_path = tempdir.path().join("svc.sock");

    // 2. Bind the socket BEFORE spawning the worker so the connect does not
    //    race — the worker's `UnixStream::connect` retries inside tokio, but
    //    binding first is simpler.
    let listener = UnixListener::bind(&socket_path).expect("bind unix listener");

    // 3. Prepare the spec.
    let spec = TaskRunSpec {
        task_id: "task-abc".into(),
        project_id: "proj-xyz".into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/task-abc".into(),
        flow: SupervisorFlow::Planning,
        model_id_per_role: HashMap::new(),
    };
    let spec_bytes = bincode::serialize(&spec).expect("serialize spec");

    // 4. Spawn the worker binary.  `CARGO_BIN_EXE_djinn-agent-worker` is
    //    populated by cargo for integration tests in the same package.
    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let mut child = tokio::process::Command::new(exe)
        .arg("--ipc-socket")
        .arg(&socket_path)
        .arg("--workspace-path")
        .arg(tempdir.path())
        .env("RUST_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    // 5. Ship the spec on stdin and close it so the worker's `read_to_end`
    //    completes.
    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(&spec_bytes).await.expect("write spec");
        stdin.shutdown().await.ok();
    }

    // 6. Accept the worker's connection and drive exactly one LoadTask RPC.
    let (stream, _addr) = listener.accept().await.expect("accept");
    let (mut read, mut write) = stream.into_split();
    let req_frame: Frame = read_frame(&mut read).await.expect("read request frame");
    match &req_frame.payload {
        FramePayload::Rpc(ServiceRpcRequest::LoadTask { task_id }) => {
            assert_eq!(task_id, "task-abc", "worker sent wrong task_id");
        }
        other => panic!("expected LoadTask, got {other:?}"),
    }

    let reply = Frame {
        correlation_id: req_frame.correlation_id,
        payload: FramePayload::RpcReply(ServiceRpcResponse::LoadTask(Ok(fixture_task("task-abc")))),
    };
    write_frame(&mut write, &reply).await.expect("write reply");

    // 7. Wait for the worker to exit and collect output.  The worker
    //    cancels its cancellation token after writing the terminal report,
    //    which closes the write half of the socket.  Drop the read half so
    //    EOF propagates to the worker's reader loop and it exits cleanly.
    drop(read);
    drop(write);

    let output = child
        .wait_with_output()
        .await
        .expect("wait for worker exit");
    assert!(
        output.status.success(),
        "worker exited non-zero: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // 8. The terminal TaskRunReport lands as a length-prefixed bincode frame
    //    on stdout (stderr carries tracing log lines — keeping them on
    //    separate fds is what lets this test decode the report without
    //    stripping log noise).
    assert!(output.stdout.len() >= 4, "stdout too short to hold a frame");
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
    // The placeholder driver synthesises an Interrupted outcome today; this
    // test's job is only to prove the wire round-trip, not the full run.
    match report.outcome {
        djinn_runtime::TaskRunOutcome::Interrupted => {}
        other => panic!("unexpected outcome: {other:?}"),
    }
}
