//! Phase 10 — end-to-end verification of the worker's cancellation path.
//!
//! Asserts that a host-initiated `FramePayload::Control(ControlMsg::Cancel)`
//! frame, sent over the live TCP connection, propagates from the worker's
//! reader loop into the `CancellationToken` that
//! [`djinn_agent_worker::worker_services::WorkerSupervisorServices`] hands the
//! supervisor, causing the supervisor to short-circuit with
//! [`djinn_runtime::TaskRunOutcome::Interrupted`] and exit cleanly.
//!
//! ## Dolt dependency
//!
//! Same constraint as `in_pod_drive.rs`: the worker bootstraps an in-Pod
//! `Database` via `bootstrap_warm_database()`, so a live Postgres at
//! `127.0.0.1:5433` (or `DJINN_TEST_DATABASE_URL`) is required — matches
//! the `make test` convention shared with `djinn-agent`'s
//! `phase1_supervisor` integration test.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use djinn_core::models::{SessionRecord, Task, TaskRunStatus, TaskRunTrigger};
use djinn_runtime::{
    ControlMsg, ResolvedCredentials, RoleKind, SerializableCredential, SupervisorFlow, TaskRunSpec,
    WorkerEvent,
};
use djinn_supervisor::services::SerializableCreateSessionParams;
use djinn_supervisor::{
    AuthHelloMsg, AuthResultMsg, BranchPublicationResult, Frame, FramePayload, ServiceRpcRequest,
    ServiceRpcResponse, TaskRunOutcome,
};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot};

// ── Fixtures ────────────────────────────────────────────────────────────────

fn fixture_task(task_id: &str, project_id: &str) -> Task {
    Task {
        id: task_id.to_string(),
        project_id: project_id.to_string(),
        short_id: "T-1".into(),
        epic_id: None,
        title: "cancel-path fixture".into(),
        description: "exercise the worker's Control(Cancel) preemption path".into(),
        design: "".into(),
        issue_type: "task".into(),
        status: "open".into(),
        priority: 0,
        owner: "test-owner".into(),
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
            refinement_run_id: None,
            refinement_intent_id: None,
            refinement_generation: None,
            refinement_round: None,
            refinement_phase: None,
            refinement_role: None,
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

/// Same source-repo + mirror seed as `in_pod_drive.rs`. Kept inline (not
/// extracted) to avoid reshaping the existing test surface; the bodies are
/// short and the duplication is intentional.
async fn make_source_repo(path: &Path) {
    run_git(&["git", "init", "-b", "main"], path).await;
    run_git(&["git", "config", "user.email", "test@example.com"], path).await;
    run_git(&["git", "config", "user.name", "Test"], path).await;
    tokio::fs::write(path.join("README.md"), "hello")
        .await
        .unwrap();
    run_git(&["git", "add", "."], path).await;
    run_git(&["git", "commit", "-m", "init"], path).await;
}

/// In-memory record of the RPCs the worker fired + the terminal report it
/// emitted.  `open_pr_calls` is the load-bearing negative assertion: an
/// interrupted run must NOT reach the merge-landing branch.
#[derive(Debug, Default)]
struct CancelAuditLog {
    load_task: usize,
    create_task_run: usize,
    update_task_run_status: Vec<TaskRunStatus>,
    open_pr_calls: usize,
    terminal_report: Option<djinn_runtime::TaskRunReport>,
}

fn write_bin<T: serde::Serialize>(path: &Path, value: &T) {
    let bytes = bincode::serialize(value).expect("bincode serialize");
    std::fs::write(path, bytes).expect("write file");
}

fn write_token(path: &Path, token: &str) {
    std::fs::write(path, token).expect("write token file");
}

/// Spin up a fake djinn-server.
///
/// The server replies to every RPC immediately *except* that on receipt of
/// `LoadTask` it appends a `Control(Cancel)` frame to the outbound stream
/// **immediately after** the `LoadTask` reply, in the same TCP write
/// ordering.  The worker's reader loop is single-threaded: it processes
/// the `LoadTask` reply first (waking the supervisor's `load_task.await`),
/// then processes the `Control(Cancel)` frame (firing `cancel.cancel()`
/// on the shared `CancellationToken`).  By the time the supervisor task
/// wakes and reaches the `for &role_kind in sequence` loop header, the
/// token is already cancelled — `cancel.is_cancelled()` short-circuits
/// to `TaskRunOutcome::Interrupted` without ever entering a stage.
///
/// This avoids the harder-to-test "cancel arrived mid-roundtrip" path,
/// which is independently exercised by the reader_loop pending-drain
/// fix landed alongside this test.
async fn start_fake_server(
    canned_project_id: String,
    expected_token: String,
    expected_task_run_id: String,
    audit: Arc<Mutex<CancelAuditLog>>,
    load_task_seen_tx: oneshot::Sender<()>,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    use djinn_runtime::wire::{read_frame, write_frame};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw listener");
    let addr = listener.local_addr().expect("local_addr");

    let handle = tokio::spawn(async move {
        let (stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "fake server accept failed");
                return;
            }
        };
        // Split so the test driver can inject an out-of-band Control(Cancel)
        // frame from a parallel task without racing the reply path.
        let (mut read_half, write_half) = stream.into_split();
        let write_half = Arc::new(Mutex::new(write_half));

        // 1. AuthHello → AuthResult { accepted: true }
        let hello: Frame = read_frame(&mut read_half).await.expect("read AuthHello");
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
        write_frame(&mut *write_half.lock().await, &ack)
            .await
            .expect("write ack");

        // 2. Dispatch loop — every RPC the supervisor body issues runs to
        //    completion immediately.  On `LoadTask` the loop signals the
        //    test driver (proof the worker reached the supervisor body)
        //    and, after writing the reply, also writes a
        //    `Control(Cancel)` frame so the worker's reader observes the
        //    cancel before the supervisor's task wakes from `load_task
        //    .await` and enters the stage for-loop.
        let mut load_task_seen = Some(load_task_seen_tx);
        loop {
            let frame: Frame = match read_frame::<_, Frame>(&mut read_half).await {
                Ok(f) => f,
                Err(_) => break,
            };
            match frame.payload {
                FramePayload::Rpc(req) => {
                    let correlation_id = frame.correlation_id;
                    let is_load_task = matches!(req, ServiceRpcRequest::LoadTask { .. });
                    let reply_payload = match req {
                        ServiceRpcRequest::LoadTask { task_id } => {
                            audit.lock().await.load_task += 1;
                            ServiceRpcResponse::LoadTask(Ok(fixture_task(
                                &task_id,
                                &canned_project_id,
                            )))
                        }
                        ServiceRpcRequest::CreateTaskRun { params } => {
                            audit.lock().await.create_task_run += 1;
                            let _ = params;
                            ServiceRpcResponse::CreateTaskRun(Ok(()))
                        }
                        ServiceRpcRequest::UpdateTaskRunStatus { run_id, status } => {
                            let _ = run_id;
                            audit.lock().await.update_task_run_status.push(status);
                            ServiceRpcResponse::UpdateTaskRunStatus(Ok(()))
                        }
                        ServiceRpcRequest::CreateSession { params } => {
                            let SerializableCreateSessionParams {
                                project_id,
                                task_id,
                                model,
                                agent_type,
                                ..
                            } = params;
                            ServiceRpcResponse::CreateSession(Ok(SessionRecord {
                                id: "session-cancel-path".into(),
                                project_id: Some(project_id),
                                task_id,
                                model_id: model,
                                agent_type,
                                started_at: "now".into(),
                                ended_at: None,
                                status: "running".into(),
                                tokens_in: 0,
                                tokens_out: 0,
                                cache_read_tokens: 0,
                                cache_write_tokens: 0,
                                task_run_id: None,
                                title: None,
                                parked_reason: None,
                                cost_usd: None,
                                input_price_per_million_snapshot: None,
                                output_price_per_million_snapshot: None,
                                cache_read_price_per_million_snapshot: None,
                                cache_write_price_per_million_snapshot: None,
                                cost_basis: "unpriced".into(),
                                billing_source: None,
                            }))
                        }
                        ServiceRpcRequest::UpdateSessionStatus { .. } => {
                            ServiceRpcResponse::UpdateSessionStatus(Ok(()))
                        }
                        ServiceRpcRequest::FlushSessionTokens { .. } => {
                            ServiceRpcResponse::FlushSessionTokens(Ok(()))
                        }
                        ServiceRpcRequest::PublishSessionMessage { .. } => {
                            ServiceRpcResponse::PublishSessionMessage(Ok(()))
                        }
                        ServiceRpcRequest::GetEnvironmentConfig { .. } => {
                            // Cancellation-path test doesn't care about the
                            // config shape — return Err so the worker's
                            // `WorkerSupervisorServices::get_environment_config`
                            // degrades to a local `EnvironmentConfig::empty()`
                            // and keeps the test focused on the cancel
                            // signal rather than environment plumbing.
                            ServiceRpcResponse::GetEnvironmentConfig(Err(
                                "fake server: degrade to local empty config".into(),
                            ))
                        }
                        ServiceRpcRequest::GetModelContextWindow { .. } => {
                            ServiceRpcResponse::GetModelContextWindow(Ok(100_000))
                        }
                        ServiceRpcRequest::GetProviderBaseUrl { .. } => {
                            ServiceRpcResponse::GetProviderBaseUrl(Err(
                                "fake server: no catalog".into()
                            ))
                        }
                        ServiceRpcRequest::PickAnyDefaultModel => {
                            ServiceRpcResponse::PickAnyDefaultModel(Ok(None))
                        }
                        ServiceRpcRequest::ExecuteStage { .. } => {
                            panic!(
                                "worker dispatched execute_stage via RPC — \
                                 stage must run locally even on the cancellation path"
                            );
                        }
                        ServiceRpcRequest::InvokeLlm { .. } => {
                            panic!(
                                "worker dispatched invoke_llm via RPC — \
                                 provider must run locally even on the cancellation path"
                            );
                        }
                        ServiceRpcRequest::PlanMemoryIntents { .. } => {
                            panic!(
                                "worker dispatched memory planning on the cancellation path — \
                                 planning must remain disabled before stage execution"
                            );
                        }
                        ServiceRpcRequest::OpenPr { .. } => {
                            audit.lock().await.open_pr_calls += 1;
                            ServiceRpcResponse::OpenPr(TaskRunOutcome::Closed {
                                reason: "fake server open_pr stub".into(),
                            })
                        }
                        ServiceRpcRequest::EmitDjinnEvent { .. } => {
                            ServiceRpcResponse::EmitDjinnEvent(Ok(()))
                        }
                        ServiceRpcRequest::ToolGithubSearch { .. } => {
                            ServiceRpcResponse::ToolGithubSearch(Err("not wired".into()))
                        }
                        ServiceRpcRequest::ToolGithubFetchFile { .. } => {
                            ServiceRpcResponse::ToolGithubFetchFile(Err("not wired".into()))
                        }
                        ServiceRpcRequest::ToolCiJobLog { .. } => {
                            ServiceRpcResponse::ToolCiJobLog(Err("not wired".into()))
                        }
                        ServiceRpcRequest::ToolCiArtifact { .. } => {
                            ServiceRpcResponse::ToolCiArtifact(Err("not wired".into()))
                        }
                        ServiceRpcRequest::TouchActivity { .. } => {
                            ServiceRpcResponse::TouchActivity(Ok(()))
                        }
                        ServiceRpcRequest::TransitionTask { .. } => {
                            ServiceRpcResponse::TransitionTask(Ok(()))
                        }
                        ServiceRpcRequest::ReservedRemovedArbiterGate => {
                            ServiceRpcResponse::ReservedRemovedArbiterGate(Err("removed".into()))
                        }
                        ServiceRpcRequest::RecordArbiterDecision { .. } => {
                            ServiceRpcResponse::RecordArbiterDecision(Ok(()))
                        }
                        ServiceRpcRequest::StartMonitoredReopen { .. } => {
                            ServiceRpcResponse::StartMonitoredReopen(Ok(()))
                        }
                        ServiceRpcRequest::CompleteMonitoredReopen { .. } => {
                            ServiceRpcResponse::CompleteMonitoredReopen(Ok(()))
                        }
                        ServiceRpcRequest::RecordArbiterSessionTermination { .. } => {
                            ServiceRpcResponse::RecordArbiterSessionTermination(Ok(false))
                        }
                        ServiceRpcRequest::PublishBranchToGithub { .. } => {
                            ServiceRpcResponse::PublishBranchToGithub(BranchPublicationResult {
                                success: false,
                                pushed_sha: None,
                                mirror_head: String::new(),
                                attempted_github_head: String::new(),
                                pr_branch_existed: false,
                                error_class: Some("fake server".into()),
                                error_message: Some(
                                    "fake server: publish_branch_to_github stub".into(),
                                ),
                            })
                        }
                        // Lease-v1 calls are outside this fixture, but the raw-wire
                        // dispatcher must remain exhaustive for appended variants.
                        ServiceRpcRequest::QueueLease { .. } => ServiceRpcResponse::QueueLease(
                            djinn_supervisor::services::LeaseResult::LeaseUnavailable,
                        ),
                        ServiceRpcRequest::GrantLease { .. } => ServiceRpcResponse::GrantLease(
                            djinn_supervisor::services::LeaseResult::LeaseUnavailable,
                        ),
                        ServiceRpcRequest::LeaseStatus { .. } => ServiceRpcResponse::LeaseStatus(
                            djinn_supervisor::services::LeaseResult::LeaseUnavailable,
                        ),
                        ServiceRpcRequest::AbandonLease { .. } => ServiceRpcResponse::AbandonLease(
                            djinn_supervisor::services::LeaseResult::LeaseUnavailable,
                        ),
                        ServiceRpcRequest::BindLeasePod { .. } => ServiceRpcResponse::BindLeasePod(
                            djinn_supervisor::services::LeaseResult::LeaseUnavailable,
                        ),
                        ServiceRpcRequest::CancelLease { .. } => ServiceRpcResponse::CancelLease(
                            djinn_supervisor::services::LeaseResult::LeaseUnavailable,
                        ),
                        ServiceRpcRequest::ReleaseLease { .. } => ServiceRpcResponse::ReleaseLease(
                            djinn_supervisor::services::LeaseResult::LeaseUnavailable,
                        ),
                    };
                    let reply = Frame {
                        correlation_id,
                        payload: FramePayload::RpcReply(reply_payload),
                    };
                    {
                        let mut guard = write_half.lock().await;
                        // Reply writes are best-effort: once the worker's
                        // CancellationToken has fired its reader exits and
                        // the worker process may rip the TCP connection
                        // down before the server's pending reply lands.
                        // We tolerate the failure and keep reading — the
                        // worker's writer task is still pushing the
                        // post-cancel frames (update_task_run_status,
                        // TerminalReport) that the assertions need, and
                        // those land on the *server's* read half, which
                        // stays open until the worker's writer half is
                        // explicitly shut down.
                        let _ = write_frame(&mut *guard, &reply).await;
                        // After replying to LoadTask, immediately push a
                        // Control(Cancel) frame on the same write half.
                        // The worker's reader is single-threaded and
                        // processes frames in order: it will deliver the
                        // LoadTask reply to the pending oneshot, then
                        // observe Cancel and fire cancel.cancel() before
                        // accepting any further frames.  By the time the
                        // supervisor's task wakes and reaches the stage
                        // for-loop's cancel check, the token is already
                        // cancelled.
                        if is_load_task {
                            let cancel_frame = Frame {
                                correlation_id: 0,
                                payload: FramePayload::Control(ControlMsg::Cancel),
                            };
                            // If the cancel-frame write fails, the test will
                            // observe it via the eventual exit-code /
                            // audit-log assertions; nothing actionable to
                            // do here in-band.
                            let _ = write_frame(&mut *guard, &cancel_frame).await;
                            if let Some(tx) = load_task_seen.take() {
                                let _ = tx.send(());
                            }
                        }
                    }
                }
                FramePayload::Event(WorkerEvent::TerminalReport(report)) => {
                    audit.lock().await.terminal_report = Some(report);
                }
                FramePayload::Event(_) => {}
                other => {
                    tracing::warn!(?other, "fake server: unexpected frame");
                }
            }
        }
    });

    (addr, handle)
}

// ── Test ────────────────────────────────────────────────────────────────────

/// Drive the worker against a fake TCP server, send `Control(Cancel)` while
/// the worker is parked on a pending `create_task_run` reply, then assert it
/// exits cleanly with a `TaskRunOutcome::Interrupted` terminal report and
/// never reached `open_pr`.
///
/// Needs Dolt at `127.0.0.1:3307` (the `make test` test instance) because
/// the worker bootstraps an in-Pod `Database` via
/// `bootstrap_warm_database()`. Same constraint as `in_pod_drive.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_observes_host_initiated_cancel() {
    // 1. Source repo + bare mirror.
    let source_dir = TempDir::new().expect("tempdir source");
    make_source_repo(source_dir.path()).await;
    let source_url = format!("file://{}", source_dir.path().display());

    let mirrors_root = TempDir::new().expect("tempdir mirrors");
    let project_id = "proj-cancel-path";
    let mirror = djinn_workspace::MirrorManager::new(mirrors_root.path().to_path_buf());
    mirror
        .ensure_mirror(project_id, &source_url)
        .await
        .expect("ensure_mirror");

    // 2. Spec + credentials + token files.
    let cfg_dir = TempDir::new().expect("tempdir cfg");
    let workspace_dir = TempDir::new().expect("tempdir workspace");

    let task_id = "task-cancel-path";
    let task_run_id = "run-cancel-path";
    let bearer = "fake-bearer-cancel";

    // Planning = single Planner stage; the supervisor short-circuits with
    // Interrupted *before* the stage starts (cancel check runs at the top
    // of the for-loop iteration), so we never reach the local provider
    // build.
    let mut per_role = HashMap::new();
    per_role.insert(RoleKind::Planner, "openai/gpt-4o".to_string());
    let spec = TaskRunSpec {
        task_run_id: format!("run-{task_id}"),
        task_attempt_id: None,
        task_id: task_id.into(),
        project_id: project_id.into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/cancel-path".into(),
        flow: SupervisorFlow::Planning,
        model_id_per_role: per_role,
        read_source_project_ids: Vec::new(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
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

    // 3. Fake server + a rendezvous channel that fires once the server has
    //    written its LoadTask reply *and* the trailing Control(Cancel)
    //    frame.  The test driver only needs to know cancellation has been
    //    queued on the wire — the supervisor's exit assertion handles the
    //    rest.
    let audit: Arc<Mutex<CancelAuditLog>> = Arc::new(Mutex::new(CancelAuditLog::default()));
    let (load_task_seen_tx, load_task_seen_rx) = oneshot::channel::<()>();
    let (addr, server) = start_fake_server(
        project_id.into(),
        bearer.into(),
        task_run_id.into(),
        audit.clone(),
        load_task_seen_tx,
    )
    .await;

    // 4. Provision a migrated per-test Postgres database for the worker
    //    subprocess. The worker's `bootstrap_warm_database()` calls the
    //    lock-free `verify_and_mark_initialized()`, which never runs
    //    migrations (the Helm pre-upgrade Job does that in production), so
    //    the bare `…/postgres` admin DB would make it die at boot with
    //    "_sqlx_migrations table missing". `open_in_memory()` + a query to
    //    force the `djinn_test_template` clone gives us a migrated per-test
    //    DSN we read off `bootstrap_info().target`. Keep the handle alive
    //    so the DB persists for the subprocess.
    let worker_db = djinn_db::Database::open_in_memory().expect("allocate per-test worker db");
    worker_db
        .table_exists("tasks")
        .await
        .expect("clone djinn_test_template into per-test worker db");
    let test_db_url = worker_db.bootstrap_info().target.clone();

    // 4b. Spawn worker against the migrated per-test database.
    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let mut child = Command::new(exe)
        .arg("task-run")
        .env("DJINN_SERVER_ADDR", addr.to_string())
        .env("DJINN_SPEC_PATH", &spec_path)
        .env("DJINN_CREDENTIALS_PATH", &creds_path)
        .env("DJINN_TOKEN_PATH", &token_path)
        .env("DJINN_TASK_RUN_ID", task_run_id)
        .env("DJINN_WORKSPACE_PATH", workspace_dir.path())
        .env("DJINN_MIRROR_ROOT", mirrors_root.path())
        .env("DJINN_DATABASE_URL", test_db_url)
        .env("RUST_LOG", "info,djinn_agent=warn,sqlx=warn")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn worker");

    // Drain stderr into a shared buffer so the timeout-panic branch can
    // still print what the worker was doing when it stalled.
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut stderr_pipe) = child.stderr.take() {
        let buf = stderr_buf.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut chunk = [0u8; 4096];
            loop {
                match stderr_pipe.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => buf.lock().await.extend_from_slice(&chunk[..n]),
                }
            }
        });
    }

    // 5. Wait for proof the server has both replied to LoadTask and
    //    queued the Control(Cancel) frame on the wire.  The supervisor's
    //    stage-loop cancel check at the next `for &role_kind in
    //    sequence` iteration is what flips the outcome to Interrupted.
    tokio::time::timeout(Duration::from_secs(30), load_task_seen_rx)
        .await
        .expect("worker should reach load_task within 30s")
        .expect("load_task rendezvous channel dropped");

    // 6. Worker should exit cleanly within 30s after we cancel.  On
    //    timeout, dump captured stderr so the failure is debuggable.
    let status = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
        Ok(res) => res.expect("collect worker status"),
        Err(_elapsed) => {
            let stderr = String::from_utf8_lossy(&stderr_buf.lock().await.clone()).to_string();
            panic!(
                "worker did not exit within 30s of cancellation\n\
                 --- captured stderr ---\n{stderr}"
            );
        }
    };

    let stderr = String::from_utf8_lossy(&stderr_buf.lock().await.clone()).to_string();
    assert!(
        status.success(),
        "worker exited non-zero after cancellation: status={:?}\n\
         --- captured stderr ---\n{stderr}",
        status
    );

    // 7. Let the fake server wind down, then audit.
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    let log = audit.lock().await;

    assert!(
        log.create_task_run >= 1,
        "expected at least one create_task_run RPC before cancellation\n\
         --- captured stderr ---\n{stderr}"
    );
    assert_eq!(
        log.open_pr_calls, 0,
        "interrupted run must never reach open_pr; saw {} call(s)\n\
         --- captured stderr ---\n{stderr}",
        log.open_pr_calls
    );
    assert!(
        log.update_task_run_status
            .iter()
            .any(|s| matches!(s, TaskRunStatus::Interrupted)),
        "supervisor must record terminal status = Interrupted; saw {:?}\n\
         --- captured stderr ---\n{stderr}",
        log.update_task_run_status
    );

    // TerminalReport delivery on the cancellation path is best-effort:
    // by the time the supervisor reaches `emit_event(TerminalReport)`,
    // the writer task may already have taken its `cancelled()` branch
    // (the writer's drain-on-cancel flushes any frame already buffered
    // at cancel time, but a frame queued *after* the writer exits can't
    // be delivered).  When the report does arrive, assert its shape;
    // when it doesn't, the host's `KubernetesRuntime::teardown` falls
    // back to Job-status polling, which observes the `Interrupted`
    // `task_runs.status` row already written above.
    if let Some(report) = log.terminal_report.as_ref() {
        assert!(
            matches!(report.outcome, TaskRunOutcome::Interrupted),
            "terminal outcome must be Interrupted; got {:?}",
            report.outcome
        );
    }
}
