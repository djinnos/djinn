//! End-to-end integration tests for the real RPC wire.
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`
//! swapped the worker transport to TCP + bearer-token handshake.  Phase 2.1
//! additionally moved the terminal `TaskRunReport` off of stdout and onto the
//! shared RPC channel as a `WorkerEvent::TerminalReport` frame.
//!
//! Test surface:
//!
//! - [`worker_exits_nonzero_when_tcp_auth_rejects`] — auth rejection.  The
//!   validator is pinned to a different token; the server answers
//!   `AuthResult { accepted: false, .. }` and closes the socket; the
//!   worker propagates the rejection as a non-zero exit.  The happy-path
//!   round-trip lived in this file pre-Phase-7b but its assertions
//!   (`TaskRunOutcome::Interrupted` against the deleted
//!   `drive_placeholder` shim) no longer match the worker's real
//!   `TaskRunSupervisor`-backed drive; see `tests/in_pod_drive.rs` for
//!   Phase 7b's end-to-end coverage of that surface.
//!
//! The server-level unit tests in `djinn_supervisor::services::server::tests`
//! already cover the byte-level handshake semantics against raw
//! `TcpStream`s; this integration test exists to verify that the
//! `djinn-agent-worker` binary's env-plumbing + `RpcServices::connect_tcp`
//! glue matches what the Pod manifest projects.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;

use djinn_core::models::{Task, TaskRunTrigger};
use djinn_runtime::{ResolvedCredentials, SupervisorFlow, TaskRunSpec};
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

fn fixture_spec(task_id: &str) -> TaskRunSpec {
    TaskRunSpec {
        task_run_id: format!("run-{task_id}"),
        task_attempt_id: None,
        task_id: task_id.into(),
        project_id: "proj-xyz".into(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: format!("djinn/{task_id}"),
        flow: SupervisorFlow::Planning,
        model_id_per_role: HashMap::new(),
        read_source_project_ids: Vec::new(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    }
}

/// Minimal host-side [`SupervisorServices`] used by
/// [`worker_exits_nonzero_when_tcp_auth_rejects`] — the worker tears the
/// connection down on `AuthResult { accepted: false }` before any RPC
/// method is invoked, so every trait method `unimplemented!()`s.
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
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn open_pr(&self, _spec: &TaskRunSpec, _task: &Task) -> TaskRunOutcome {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn create_task_run(
        &self,
        _params: djinn_supervisor::SerializableCreateTaskRunParams,
    ) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn update_task_run_status(
        &self,
        _run_id: String,
        _status: djinn_core::models::TaskRunStatus,
    ) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn get_model_context_window(&self, _model_id: String) -> Result<i64, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn get_provider_base_url(&self, _catalog_provider_id: String) -> Result<String, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn create_session(
        &self,
        _params: djinn_supervisor::services::SerializableCreateSessionParams,
    ) -> Result<djinn_core::models::SessionRecord, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn publish_session_message(
        &self,
        _session_id: String,
        _task_id: String,
        _agent_type: String,
        _message: serde_json::Value,
    ) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn get_environment_config(
        &self,
        _project_id: String,
    ) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn invoke_llm(
        &self,
        _model_id: String,
        _conversation: djinn_provider::message::Conversation,
        _tools: Vec<serde_json::Value>,
        _tool_choice: Option<djinn_provider::provider::ToolChoice>,
    ) -> Result<djinn_provider::provider::LlmResponse, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

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
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn emit_djinn_event(
        &self,
        _event: djinn_supervisor::services::SerializableDjinnEvent,
    ) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn tool_github_search(
        &self,
        _project_id: Option<String>,
        _arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn tool_github_fetch_file(
        &self,
        _project_id: Option<String>,
        _arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn tool_ci_job_log(
        &self,
        _session_task_id: Option<String>,
        _arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn touch_activity(&self, _task_id: String) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn transition_task(
        &self,
        _task_id: String,
        _action: String,
        _reason: Option<String>,
    ) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn record_arbiter_decision(
        &self,
        _task_id: String,
        _decision: String,
        _evidence_json: String,
    ) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn start_monitored_reopen(
        &self,
        _task_id: String,
        _directive: String,
        _verification_command: String,
        _exclude_models: Vec<String>,
    ) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn complete_monitored_reopen(&self, _task_id: String) -> Result<(), String> {
        unimplemented!("not exercised on the auth-rejection path")
    }

    async fn record_arbiter_session_termination(
        &self,
        _task_id: String,
        _is_infra_failure: bool,
    ) -> Result<bool, String> {
        unimplemented!("not exercised on the auth-rejection path")
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

/// Serialize `spec` to a tempfile, return the file's path so the worker
/// can be pointed at it via `DJINN_SPEC_PATH`.
fn write_spec(dir: &std::path::Path, spec: &TaskRunSpec) -> std::path::PathBuf {
    let path = dir.join("spec.bin");
    let bytes = bincode::serialize(spec).expect("serialize spec");
    std::fs::write(&path, bytes).expect("write spec file");
    path
}

/// Drop an empty `ResolvedCredentials` bincode blob next to the spec.
/// Phase 7a made `DJINN_CREDENTIALS_PATH` a mandatory input — the worker
/// reads it BEFORE dialling the host, so without a real file present the
/// auth-rejection path is unreachable.  The empty bundle is a valid wire
/// value: the worker logs `roles=[]` at start-up and proceeds to the
/// handshake, which is the only path this test cares about.
fn write_empty_credentials(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("credentials.bin");
    let creds = ResolvedCredentials::default();
    let bytes = bincode::serialize(&creds).expect("serialize empty credentials");
    std::fs::write(&path, bytes).expect("write credentials file");
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
    let credentials_path = write_empty_credentials(tempdir.path());
    let token_path = write_token(tempdir.path(), bogus_bearer);

    let exe = env!("CARGO_BIN_EXE_djinn-agent-worker");
    let child = tokio::process::Command::new(exe)
        .arg("task-run")
        .env("DJINN_SERVER_ADDR", addr.to_string())
        .env("DJINN_SPEC_PATH", &spec_path)
        .env("DJINN_CREDENTIALS_PATH", &credentials_path)
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
