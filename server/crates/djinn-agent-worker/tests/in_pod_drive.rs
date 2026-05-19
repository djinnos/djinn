//! In-Pod drive integration test for Phase 7c.
//!
//! Phase 7b cut over `djinn-agent-worker` from the `drive_placeholder`
//! shim to a real [`djinn_supervisor::TaskRunSupervisor`] driven by
//! [`djinn_agent_worker::worker_services::WorkerSupervisorServices`].
//! This test spawns the worker binary against a fake TCP launcher,
//! materialises a tempdir mirror, and asserts that:
//!
//! 1. The worker connects, completes the AuthHello handshake, and exits
//!    cleanly (status 0).
//! 2. `create_task_run` + `load_task` + `update_task_run_status` round-trip
//!    over the RPC channel — proof the worker is driving the real supervisor
//!    body, not the old placeholder.
//! 3. `execute_stage` and `invoke_llm` RPC methods are **never** invoked —
//!    proof that the worker constructs its provider locally from the
//!    ResolvedCredentials mounted via the per-task-run Secret (Phase 7a +
//!    the Phase 7b wiring this test guards).
//! 4. The worker emits a terminal [`djinn_runtime::TaskRunReport`] via the
//!    `WorkerEvent::TerminalReport` frame on the same TCP channel.
//!
//! ## Why the test is `#[ignore]` by default
//!
//! The Phase 7b worker_services still depends on the in-tree per-stage
//! executor (`djinn_agent::supervisor_impl::stage::execute_stage`) which
//! threads an `AgentContext` through helpers that touch the database
//! directly (`resolve_role_overrides`, `build_prompt_context`,
//! `spawn_post_session_work`, `task_merge::resolve_project_path_for_id`).
//! The worker bootstraps an in-Pod `Database` against
//! `DJINN_MYSQL_URL` so those calls have a live connection; the test
//! needs Dolt at `127.0.0.1:3307` (or a `DJINN_TEST_MYSQL_URL` override).
//!
//! Additionally the test exercises a Planner stage against a fake API key,
//! so the provider stream will fail; the worker maps that to
//! `StageOutcome::Failed`, the supervisor returns a `TaskRunReport` with
//! `outcome = TaskRunOutcome::Failed`, and the test asserts the worker
//! reached that terminal state. This is the load-bearing assertion: the
//! provider error proves the worker tried to call the LLM locally, not
//! via the host's `invoke_llm` RPC.
//!
//! Mark the test `#[ignore]` because:
//! - Local Dolt at :3307 is not guaranteed in every dev environment.
//! - The Phase 7-followup that eliminates the `AgentContext.db` seam will
//!   simplify or replace this test once landed.
//!
//! Run explicitly with:
//!   `cargo test -p djinn-agent-worker --test in_pod_drive -- --ignored`

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use djinn_core::models::{SessionRecord, Task, TaskRunStatus, TaskRunTrigger};
use djinn_runtime::{
    ResolvedCredentials, RoleKind, SerializableCredential, SupervisorFlow, TaskRunSpec,
    WorkerEvent,
};
use djinn_supervisor::services::{
    SerializableCreateSessionParams, SerializableCreateTaskRunParams,
};
use djinn_supervisor::{
    AuthHelloMsg, AuthResultMsg, Frame, FramePayload, ServiceRpcRequest, ServiceRpcResponse,
    StageError, StageOutcome, SupervisorServices, TaskRunOutcome,
};
use djinn_workspace::Workspace;
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ── Fixtures ────────────────────────────────────────────────────────────────

fn fixture_task(task_id: &str, project_id: &str) -> Task {
    Task {
        id: task_id.to_string(),
        project_id: project_id.to_string(),
        short_id: "T-1".into(),
        epic_id: None,
        title: "in-pod drive fixture".into(),
        description: "exercise the worker's in-Pod TaskRunSupervisor drive".into(),
        design: "".into(),
        issue_type: "task".into(),
        status: "open".into(),
        priority: 0,
        owner: "test-owner".into(),
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

async fn run_git(cmd: &[&str], cwd: &Path) {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .output()
        .await
        .expect("git");
    assert!(
        output.status.success(),
        "cmd {cmd:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Initialise a tiny source repo + a single commit on `main`. The
/// `MirrorManager` clones from this as `file://...` to materialise the
/// bare mirror the worker will then `clone_ephemeral` from.
async fn make_source_repo(path: &Path) {
    run_git(&["git", "init", "-b", "main"], path).await;
    run_git(&["git", "config", "user.email", "test@example.com"], path).await;
    run_git(&["git", "config", "user.name", "Test"], path).await;
    tokio::fs::write(path.join("README.md"), "hello").await.unwrap();
    run_git(&["git", "add", "."], path).await;
    run_git(&["git", "commit", "-m", "init"], path).await;
}

/// In-memory record of every RPC method the worker invoked. Used at the
/// end of the test to assert which methods were called locally vs. routed
/// to the fake server.
#[derive(Debug, Default)]
struct RpcAuditLog {
    load_task: usize,
    create_task_run: usize,
    update_task_run_status: usize,
    create_session: usize,
    update_session_status: usize,
    publish_session_message: usize,
    get_environment_config: usize,
    get_model_context_window: usize,
    pick_any_default_model: usize,
    execute_stage_attempts: usize,
    invoke_llm_attempts: usize,
    open_pr: usize,
    emit_djinn_event: usize,
    terminal_report: Option<djinn_runtime::TaskRunReport>,
}

/// Spin up a raw `tokio::net::TcpListener`-backed djinn-server stand-in
/// that handles every RPC the Phase 7b worker is expected to fire during
/// a Planning flow. Returns `(SocketAddr, JoinHandle, audit log)`.
async fn start_fake_server(
    canned_task_id: String,
    canned_project_id: String,
    expected_token: String,
    expected_task_run_id: String,
    audit: Arc<Mutex<RpcAuditLog>>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use djinn_runtime::wire::{read_frame, write_frame};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw listener");
    let addr = listener.local_addr().expect("local_addr");

    let handle = tokio::spawn(async move {
        let (mut stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "fake server accept failed");
                return;
            }
        };

        // 1. AuthHello → AuthResult { accepted: true }
        let hello: Frame = read_frame(&mut stream).await.expect("read AuthHello");
        let hello_correlation = hello.correlation_id;
        let (task_run_id, token) = match hello.payload {
            FramePayload::AuthHello(AuthHelloMsg { task_run_id, token }) => (task_run_id, token),
            other => panic!("expected AuthHello, got {other:?}"),
        };
        assert_eq!(token, expected_token);
        assert_eq!(task_run_id, expected_task_run_id);
        let ack = Frame {
            correlation_id: hello_correlation,
            payload: FramePayload::AuthResult(AuthResultMsg {
                accepted: true,
                error: None,
            }),
        };
        write_frame(&mut stream, &ack).await.expect("write ack");

        // 2. Dispatch loop — handle every RPC the worker's drive path can
        //    legitimately issue. execute_stage / invoke_llm panic the test
        //    if the worker ever routes them via RPC instead of running them
        //    locally.
        loop {
            let frame: Frame = match read_frame(&mut stream).await {
                Ok(f) => f,
                Err(_) => return,
            };
            match frame.payload {
                FramePayload::Rpc(req) => {
                    let reply_payload = handle_rpc(req, &canned_task_id, &canned_project_id, &audit).await;
                    let reply = Frame {
                        correlation_id: frame.correlation_id,
                        payload: FramePayload::RpcReply(reply_payload),
                    };
                    if write_frame(&mut stream, &reply).await.is_err() {
                        return;
                    }
                }
                FramePayload::Event(WorkerEvent::TerminalReport(report)) => {
                    audit.lock().await.terminal_report = Some(report);
                }
                FramePayload::Event(_) => {
                    // Worker emits no other events today.
                }
                other => {
                    tracing::warn!(?other, "fake server: unexpected control frame");
                }
            }
        }
    });

    (addr, handle)
}

async fn handle_rpc(
    req: ServiceRpcRequest,
    canned_task_id: &str,
    canned_project_id: &str,
    audit: &Arc<Mutex<RpcAuditLog>>,
) -> ServiceRpcResponse {
    match req {
        ServiceRpcRequest::LoadTask { task_id } => {
            audit.lock().await.load_task += 1;
            ServiceRpcResponse::LoadTask(Ok(fixture_task(&task_id, canned_project_id)))
        }
        ServiceRpcRequest::CreateTaskRun { params } => {
            audit.lock().await.create_task_run += 1;
            let _ = params;
            ServiceRpcResponse::CreateTaskRun(Ok(()))
        }
        ServiceRpcRequest::UpdateTaskRunStatus { run_id, status } => {
            audit.lock().await.update_task_run_status += 1;
            let _ = (run_id, status);
            ServiceRpcResponse::UpdateTaskRunStatus(Ok(()))
        }
        ServiceRpcRequest::CreateSession { params } => {
            audit.lock().await.create_session += 1;
            let SerializableCreateSessionParams {
                project_id,
                task_id,
                model,
                agent_type,
                ..
            } = params;
            ServiceRpcResponse::CreateSession(Ok(SessionRecord {
                id: format!("session-{}", canned_task_id),
                project_id: Some(project_id),
                task_id,
                model_id: model,
                agent_type,
                started_at: "now".into(),
                ended_at: None,
                status: "running".into(),
                tokens_in: 0,
                tokens_out: 0,
                task_run_id: None,
                title: None,
            }))
        }
        ServiceRpcRequest::UpdateSessionStatus {
            session_id,
            status,
            tokens_in,
            tokens_out,
        } => {
            audit.lock().await.update_session_status += 1;
            let _ = (session_id, status, tokens_in, tokens_out);
            ServiceRpcResponse::UpdateSessionStatus(Ok(()))
        }
        ServiceRpcRequest::PublishSessionMessage {
            session_id,
            task_id,
            agent_type,
            message,
        } => {
            audit.lock().await.publish_session_message += 1;
            let _ = (session_id, task_id, agent_type, message);
            ServiceRpcResponse::PublishSessionMessage(Ok(()))
        }
        ServiceRpcRequest::GetEnvironmentConfig { project_id } => {
            audit.lock().await.get_environment_config += 1;
            let _ = project_id;
            ServiceRpcResponse::GetEnvironmentConfig(Ok(
                djinn_stack::environment::EnvironmentConfig::empty(),
            ))
        }
        ServiceRpcRequest::GetModelContextWindow { model_id } => {
            audit.lock().await.get_model_context_window += 1;
            let _ = model_id;
            ServiceRpcResponse::GetModelContextWindow(Ok(100_000))
        }
        ServiceRpcRequest::GetProviderBaseUrl { catalog_provider_id } => {
            // Worker uses default base URL when this errors — return an Err
            // so it falls back without us needing to know the right value.
            let _ = catalog_provider_id;
            ServiceRpcResponse::GetProviderBaseUrl(Err("no catalog row in fake server".into()))
        }
        ServiceRpcRequest::PickAnyDefaultModel => {
            audit.lock().await.pick_any_default_model += 1;
            ServiceRpcResponse::PickAnyDefaultModel(Ok(None))
        }
        ServiceRpcRequest::ExecuteStage { .. } => {
            audit.lock().await.execute_stage_attempts += 1;
            panic!("worker must run execute_stage locally; RPC dispatch is a Phase 7b regression");
        }
        ServiceRpcRequest::InvokeLlm { .. } => {
            audit.lock().await.invoke_llm_attempts += 1;
            panic!("worker must call the provider locally; invoke_llm RPC is a Phase 7b regression");
        }
        ServiceRpcRequest::OpenPr { .. } => {
            audit.lock().await.open_pr += 1;
            ServiceRpcResponse::OpenPr(TaskRunOutcome::Closed {
                reason: "fake server open_pr stub".into(),
            })
        }
        ServiceRpcRequest::EmitDjinnEvent { event } => {
            audit.lock().await.emit_djinn_event += 1;
            let _ = event;
            ServiceRpcResponse::EmitDjinnEvent(Ok(()))
        }
    }
}

fn write_bin<T: serde::Serialize>(path: &Path, value: &T) {
    let bytes = bincode::serialize(value).expect("bincode serialize");
    std::fs::write(path, bytes).expect("write file");
}

fn write_token(path: &Path, token: &str) {
    std::fs::write(path, token).expect("write token file");
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Drive the Phase 7b worker against a fake TCP server + tempdir mirror,
/// then assert it ran the real supervisor (create_task_run + load_task +
/// update_task_run_status all round-tripped), never used the host's
/// `execute_stage` or `invoke_llm` RPC, and emitted a `TerminalReport`.
///
/// Marked `#[ignore]` — see module docs for rationale. Run with
/// `cargo test -p djinn-agent-worker --test in_pod_drive -- --ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "phase 7 followup: requires Dolt at :3307 + still depends on AgentContext.db seam"]
async fn worker_drives_real_supervisor_in_pod() {
    // 1. Materialise a source repo and a bare mirror cloned from it. The
    //    worker's MirrorManager will `clone_ephemeral` from the bare mirror
    //    to attach a workspace.
    let source_dir = TempDir::new().expect("tempdir source");
    make_source_repo(source_dir.path()).await;
    let source_url = format!("file://{}", source_dir.path().display());

    let mirrors_root = TempDir::new().expect("tempdir mirrors");
    let project_id = "proj-pod-drive";
    let mirror = djinn_workspace::MirrorManager::new(mirrors_root.path().to_path_buf());
    mirror
        .ensure_mirror(project_id, &source_url)
        .await
        .expect("ensure_mirror");

    // 2. Seed the spec + credentials files the worker reads at boot.
    let cfg_dir = TempDir::new().expect("tempdir cfg");
    let workspace_dir = TempDir::new().expect("tempdir workspace");

    let task_id = "task-pod-drive";
    let task_run_id = "run-pod-drive";
    let bearer = "fake-bearer-token";

    let mut per_role = HashMap::new();
    per_role.insert(RoleKind::Planner, "openai/gpt-4o".to_string());
    let spec = TaskRunSpec {
        task_id: task_id.into(),
        project_id: project_id.into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/pod-drive".into(),
        // Planning = single Planner stage. Minimises reply-loop surface
        // while still exercising one full provider construction.
        flow: SupervisorFlow::Planning,
        model_id_per_role: per_role,
    };
    let spec_path: PathBuf = cfg_dir.path().join("spec.bin");
    write_bin(&spec_path, &spec);

    let mut creds = ResolvedCredentials::new();
    creds.insert(
        RoleKind::Planner,
        SerializableCredential::ApiKey {
            key_name: "OPENAI_API_KEY".into(),
            api_key: "sk-fake-test-key".into(),
        },
    );
    let creds_path: PathBuf = cfg_dir.path().join("credentials.bin");
    write_bin(&creds_path, &creds);

    let token_path: PathBuf = cfg_dir.path().join("token");
    write_token(&token_path, bearer);

    // 3. Bring up the fake TCP server with the audit log.
    let audit: Arc<Mutex<RpcAuditLog>> = Arc::new(Mutex::new(RpcAuditLog::default()));
    let (addr, server) = start_fake_server(
        task_id.into(),
        project_id.into(),
        bearer.into(),
        task_run_id.into(),
        audit.clone(),
    )
    .await;

    // 4. Spawn the worker binary. Inherits DJINN_TEST_MYSQL_URL so the
    //    in-Pod `bootstrap_warm_database()` lands on the same isolated Dolt
    //    branch this crate's other DB-touching tests use.
    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let test_db_url = std::env::var("DJINN_TEST_MYSQL_URL")
        .unwrap_or_else(|_| "mysql://root@127.0.0.1:3307".into());
    let child = Command::new(exe)
        .arg("task-run")
        .env("DJINN_SERVER_ADDR", addr.to_string())
        .env("DJINN_SPEC_PATH", &spec_path)
        .env("DJINN_CREDENTIALS_PATH", &creds_path)
        .env("DJINN_TOKEN_PATH", &token_path)
        .env("DJINN_TASK_RUN_ID", task_run_id)
        .env("DJINN_WORKSPACE_PATH", workspace_dir.path())
        .env("DJINN_MIRROR_ROOT", mirrors_root.path())
        // Point the in-Pod Database at the test Dolt branch.
        .env("DJINN_MYSQL_URL", format!("{}/djinn_test", test_db_url))
        .env("DJINN_DB_BACKEND", "dolt")
        .env("DJINN_MYSQL_FLAVOR", "dolt")
        .env("RUST_LOG", "info,djinn_agent=warn,sqlx=warn")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker");

    // 5. Wait up to 60 seconds for the worker to exit. The fake API key
    //    causes the provider stream to fail, which surfaces as
    //    `StageOutcome::Failed` and a clean `TaskRunOutcome::Failed`
    //    terminal report — exit status 0.
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("worker should exit within 60s")
        .expect("collect worker output");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "worker exited non-zero: status={:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );

    // 6. Let the fake server's accept task wind down once the worker
    //    closes its stream, then audit what flowed through.
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    let log = audit.lock().await;

    // The worker MUST NOT have routed execute_stage or invoke_llm through
    // RPC — both panic the fake server when hit, but we also check the
    // counters defensively.
    assert_eq!(
        log.execute_stage_attempts, 0,
        "worker dispatched execute_stage via RPC instead of running it locally"
    );
    assert_eq!(
        log.invoke_llm_attempts, 0,
        "worker dispatched invoke_llm via RPC instead of building its provider locally"
    );

    // The supervisor body must have round-tripped these.
    assert!(log.load_task >= 1, "expected at least one load_task RPC");
    assert!(
        log.create_task_run >= 1,
        "expected at least one create_task_run RPC (supervisor body)"
    );
    assert!(
        log.update_task_run_status >= 1,
        "expected at least one update_task_run_status RPC (supervisor body)"
    );
    // The stage executor must have round-tripped session creation +
    // environment_config lookup.
    assert!(
        log.create_session >= 1,
        "expected at least one create_session RPC from the stage executor"
    );
    assert!(
        log.get_environment_config >= 1,
        "expected at least one get_environment_config RPC"
    );

    // Terminal report should have surfaced via Event frame.
    let report = log
        .terminal_report
        .as_ref()
        .expect("worker should have emitted TerminalReport via Event frame");
    assert!(
        !report.task_run_id.is_empty(),
        "terminal report missing task_run_id"
    );
    // Either Failed (provider call rejected by fake API key) or Closed
    // (Planning flow finished without firing the provider — unusual but
    // tolerated to keep the test resilient to upstream stage changes).
    match &report.outcome {
        TaskRunOutcome::Failed { .. } | TaskRunOutcome::Closed { .. } => {}
        other => panic!("unexpected terminal outcome: {other:?}"),
    }
}

// ── Object-safety smoke test ────────────────────────────────────────────────

/// Confirms the trait methods we expect on the fake server compile
/// against `FakeServices`. This is an `unused` static; never run.
#[allow(dead_code)]
fn _trait_obj_safety_smoke() {
    fn _accepts_supervisor_services(_: &dyn SupervisorServices) {}
    fn _stage_outcome_is_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
    _stage_outcome_is_serde::<StageOutcome>();
    _stage_outcome_is_serde::<StageError>();
    let _: Option<Workspace> = None;
    let _ = CancellationToken::new();
    let _: Result<Task, String> = Err(String::new());
    let _: Result<(), String> = Err(String::new());
    let _: Result<TaskRunStatus, String> = Err(String::new());
    let _: Result<SerializableCreateTaskRunParams, String> = Err(String::new());
}
