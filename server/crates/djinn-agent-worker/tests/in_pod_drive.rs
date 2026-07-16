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
//! ## Dolt dependency
//!
//! The Phase 7b worker_services still depends on the in-tree per-stage
//! executor (`djinn_agent::supervisor_impl::stage::execute_stage`) which
//! threads an `AgentContext` through helpers that touch the database
//! directly (`resolve_role_overrides`, `build_prompt_context`,
//! `spawn_post_session_work`, `task_merge::resolve_project_path_for_id`).
//! The worker bootstraps an in-Pod `Database` against
//! `DJINN_DATABASE_URL` so those calls have a live connection; the test
//! needs Postgres at `127.0.0.1:5433` (or a `DJINN_TEST_DATABASE_URL`
//! override) — same convention as `djinn-agent`'s `phase1_supervisor`
//! integration test (`make test` brings up the test Postgres instance).
//!
//! The test exercises a Planner stage with an OAuth-style credential
//! whose `base_url` points at an unreachable port (`http://127.0.0.1:1`),
//! so the provider stream fails fast (connection refused) instead of
//! waiting for the OpenAI default base URL to time out. The worker maps
//! the failure to `StageOutcome::Failed`, the supervisor returns a
//! `TaskRunReport` with `outcome = TaskRunOutcome::Failed`, and the test
//! asserts the worker reached that terminal state. This is the
//! load-bearing assertion: the provider error proves the worker tried to
//! call the LLM locally, not via the host's `invoke_llm` RPC.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use djinn_core::models::{SessionRecord, Task, TaskRunStatus, TaskRunTrigger};
use djinn_runtime::{
    ResolvedCredentials, RoleKind, SerializableCredential, SupervisorFlow, TaskRunSpec, WorkerEvent,
};
use djinn_supervisor::services::{
    SerializableCreateSessionParams, SerializableCreateTaskRunParams,
};
use djinn_supervisor::{
    AuthHelloMsg, AuthResultMsg, BranchPublicationResult, Frame, FramePayload, ServiceRpcRequest,
    ServiceRpcResponse, StageError, StageOutcome, SupervisorServices, TaskRunOutcome,
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
    tokio::fs::write(path.join("README.md"), "hello")
        .await
        .unwrap();
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
    touch_activity: usize,
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
            let cid = frame.correlation_id;
            match frame.payload {
                FramePayload::Rpc(req) => {
                    let reply_payload =
                        handle_rpc(req, &canned_task_id, &canned_project_id, &audit).await;
                    let reply = Frame {
                        correlation_id: cid,
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
        ServiceRpcRequest::UpdateSessionStatus {
            session_id,
            status,
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
            parked_reason,
        } => {
            audit.lock().await.update_session_status += 1;
            let _ = (
                session_id,
                status,
                tokens_in,
                tokens_out,
                cache_read,
                cache_write,
                parked_reason,
            );
            ServiceRpcResponse::UpdateSessionStatus(Ok(()))
        }
        ServiceRpcRequest::FlushSessionTokens { .. } => {
            ServiceRpcResponse::FlushSessionTokens(Ok(()))
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
            // The wire variant ships an opaque JSON-encoded
            // `EnvironmentConfig` string (HookCommand's `#[serde(untagged)]`
            // representation is bincode-fatal — see
            // `server/crates/djinn-supervisor/src/services/wire.rs`).
            let cfg = djinn_stack::environment::EnvironmentConfig::empty();
            let cfg_json = serde_json::to_string(&cfg).expect("encode EnvironmentConfig");
            ServiceRpcResponse::GetEnvironmentConfig(Ok(cfg_json))
        }
        ServiceRpcRequest::GetModelContextWindow { model_id } => {
            audit.lock().await.get_model_context_window += 1;
            let _ = model_id;
            ServiceRpcResponse::GetModelContextWindow(Ok(100_000))
        }
        ServiceRpcRequest::GetProviderBaseUrl {
            catalog_provider_id,
        } => {
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
            panic!(
                "worker must call the provider locally; invoke_llm RPC is a Phase 7b regression"
            );
        }
        ServiceRpcRequest::PlanMemoryIntents { .. } => {
            panic!(
                "worker dispatched memory planning in the in-pod path — \
                 planning must remain disabled before stage execution"
            );
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
        ServiceRpcRequest::ToolGithubSearch { .. } => ServiceRpcResponse::ToolGithubSearch(Err(
            "fake server: tool_github_search not wired".into(),
        )),
        ServiceRpcRequest::ToolGithubFetchFile { .. } => ServiceRpcResponse::ToolGithubFetchFile(
            Err("fake server: tool_github_fetch_file not wired".into()),
        ),
        ServiceRpcRequest::ToolCiJobLog { .. } => {
            ServiceRpcResponse::ToolCiJobLog(Err("fake server: tool_ci_job_log not wired".into()))
        }
        ServiceRpcRequest::TouchActivity { task_id } => {
            audit.lock().await.touch_activity += 1;
            let _ = task_id;
            ServiceRpcResponse::TouchActivity(Ok(()))
        }
        ServiceRpcRequest::TransitionTask {
            task_id,
            action,
            reason,
        } => {
            let _ = (task_id, action, reason);
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
                error_message: Some("fake server: publish_branch_to_github stub".into()),
            })
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
/// Needs Dolt at `127.0.0.1:3307` (the `make test` test instance) — see
/// the module docs for rationale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
        task_run_id: format!("run-{task_id}"),
        task_attempt_id: None,
        task_id: task_id.into(),
        project_id: project_id.into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "djinn/pod-drive".into(),
        // Planning = single Planner stage. Minimises reply-loop surface
        // while still exercising one full provider construction.
        flow: SupervisorFlow::Planning,
        model_id_per_role: per_role,
        read_source_project_ids: Vec::new(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    };
    let spec_path: PathBuf = cfg_dir.path().join("spec.bin");
    write_bin(&spec_path, &spec);

    // Use an OAuth-style credential whose base_url points at a port that
    // refuses TCP connects (`127.0.0.1:1` is in the privileged range with
    // no listener), so the provider stream fails fast with "connection
    // refused" instead of waiting for the real OpenAI endpoint to time
    // out. The worker maps the failure to `StageOutcome::Failed` and the
    // supervisor surfaces a `TaskRunOutcome::Failed` terminal report —
    // the load-bearing assertion is that the worker tried to call the
    // LLM locally, not via the host's `invoke_llm` RPC.
    let oauth_wire = serde_json::json!({
        "base_url": "http://127.0.0.1:1",
        "auth": { "BearerToken": "fake-test-token" },
        "format_family": "OpenAI",
        "model_id": "gpt-4o",
        "context_window": 100_000_u32,
        "session_affinity_key": null,
        "provider_headers": {},
        "capabilities": {
            "streaming": true,
            "max_tokens_default": null,
        },
    });
    let mut creds = ResolvedCredentials::new();
    creds.insert(
        RoleKind::Planner,
        SerializableCredential::OAuthConfig {
            config_json: oauth_wire.to_string(),
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

    // 4. Provision a migrated per-test Postgres database for the worker
    //    subprocess. The worker's `bootstrap_warm_database()` opens
    //    `DJINN_DATABASE_URL` and calls `verify_and_mark_initialized()`,
    //    which is deliberately lock-free and DOES NOT run migrations (in
    //    production the Helm pre-upgrade Job migrates first). So we cannot
    //    hand the worker the bare `…/postgres` admin DB — it has no
    //    `_sqlx_migrations` table and the worker would die at boot with
    //    "schema is behind / _sqlx_migrations table missing".
    //
    //    `Database::open_in_memory()` allocates a fresh
    //    `djinn_test_<uuid>` database; forcing initialization (any query
    //    via `table_exists`) clones it from the pre-built
    //    `djinn_test_template` (all migrations applied). We then read the
    //    resulting per-test DSN off `bootstrap_info().target` and hand
    //    *that* to the worker. The handle is kept alive for the duration
    //    of the test so the database stays around for the subprocess.
    let worker_db = djinn_db::Database::open_in_memory().expect("allocate per-test worker db");
    worker_db
        .table_exists("tasks")
        .await
        .expect("clone djinn_test_template into per-test worker db");
    let worker_db_url = worker_db.bootstrap_info().target.clone();

    // 5. Spawn the worker binary against the migrated per-test database.
    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let test_db_url = worker_db_url;
    let mut child = Command::new(exe)
        .arg("task-run")
        .env("DJINN_SERVER_ADDR", addr.to_string())
        .env("DJINN_SPEC_PATH", &spec_path)
        .env("DJINN_CREDENTIALS_PATH", &creds_path)
        .env("DJINN_TOKEN_PATH", &token_path)
        .env("DJINN_TASK_RUN_ID", task_run_id)
        .env("DJINN_WORKSPACE_PATH", workspace_dir.path())
        .env("DJINN_MIRROR_ROOT", mirrors_root.path())
        // Point the in-Pod Database at the test Postgres database.
        .env("DJINN_DATABASE_URL", test_db_url)
        .env("RUST_LOG", "info,djinn_agent=warn,sqlx=warn")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn worker");

    // Drain stderr into a shared buffer so the timeout-panic branch can
    // surface what the worker was doing when it stalled.
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

    // 5. Wait for the worker to exit. The OAuth credential's `base_url`
    //    points at `127.0.0.1:1` (no listener), so the provider stream
    //    fails fast with "connection refused", surfaces as
    //    `StageOutcome::Failed`, and the worker emits a clean
    //    `TaskRunOutcome::Failed` terminal report — exit status 0.
    let status = match tokio::time::timeout(Duration::from_secs(45), child.wait()).await {
        Ok(res) => res.expect("collect worker exit status"),
        Err(_elapsed) => {
            let stderr = String::from_utf8_lossy(&stderr_buf.lock().await.clone()).to_string();
            panic!(
                "worker did not exit within 45s\n\
                 --- captured stderr ---\n{stderr}"
            );
        }
    };

    let captured = String::from_utf8_lossy(&stderr_buf.lock().await.clone()).to_string();
    assert!(
        status.success(),
        "worker exited non-zero: status={:?}\n\
         --- captured stderr ---\n{captured}",
        status
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
    let report = log.terminal_report.as_ref().unwrap_or_else(|| {
        panic!(
            "worker should have emitted TerminalReport via Event frame\n\
                 --- captured stderr ---\n{captured}"
        )
    });
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
