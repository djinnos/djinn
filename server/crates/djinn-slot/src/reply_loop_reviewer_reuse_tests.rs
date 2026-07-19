//! End-to-end reviewer canonical final-verification reuse regression.
//!
//! Proves that a reviewer dispatched on the same task and complete fingerprint
//! as a post-authoring worker pass reuses it through the production reply loop
//! with zero canonical commands and zero lease requests/acquisitions. The
//! setup-time pre-verification pass remains ineligible because the worker edits
//! the tracked tree after setup, changing the complete fingerprint.
//!
//! The reviewer's `submit_review` call drives the same consult-or-run boundary
//! established by 30qu and threaded by ftni: the coordinator resolves the
//! current tree, consults the task-scoped complete-fingerprint cache, and on a
//! fresh hit injects the persisted run context without ever dispatching a
//! canonical command or requesting/acquiring the invocation lease.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::final_verification::{
    FinalVerificationCoordinatorRequest, FinalVerificationRecordingOutcome,
    FinalVerificationResolvedMaterial,
};
use crate::host::{ResolvedMcpTools, SlotContext, SlotHostCallbacks};
use crate::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::test_helpers::{
    FakeProvider, agent_context_from_db_with_callbacks, create_test_db, create_test_epic,
    create_test_project, create_test_task, test_tempdir,
};
use djinn_core::{
    canonical_verify::{
        CanonicalCommandDescriptorV1, CanonicalFinalVerificationPlanV1, CanonicalHermeticityV1,
        EnvironmentIdentityV1, ImmutableImageV1, ResolvedEnvironmentIdentityInputV1,
        ToolProbeStatus, ToolProbeV1, VerificationInputManifestV1,
    },
    models::{Task, VerifySource},
};
use djinn_db::repositories::session::{CreateSessionParams, SessionRepository};
use djinn_db::repositories::settings::SettingsRepository;
use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
use djinn_db::repositories::verify_run::{
    RecordEligibleFinalVerificationPassParams, RequiredFinalVerificationCommand,
    VerifyRunRepository,
};
use djinn_git::{
    VerificationInputFingerprint, VerificationInputFingerprintConfig,
    compute_verification_input_fingerprint_with_config, run_git_command_in,
};
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::StreamEvent;
use djinn_sandbox::final_verification_execution::FinalVerificationExecutionRequest;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Deterministic material builder (mirrors the reply-loop worker reuse harness)
// ---------------------------------------------------------------------------

fn reviewer_reuse_material(worktree: std::path::PathBuf) -> FinalVerificationResolvedMaterial {
    let required_checks = vec!["format".to_owned(), "slot-clippy".to_owned()];
    let manifest = VerificationInputManifestV1 {
        version: 1,
        repo_paths: vec![],
        environment_names: vec![],
        read_only_external_inputs: vec![],
        output_only_globs: vec![],
    };
    let commands = required_checks
        .iter()
        .map(|check_id| CanonicalCommandDescriptorV1 {
            check_id: check_id.clone(),
            executable: format!("{check_id}-tool"),
            argv: vec![check_id.clone()],
            working_directory: ".".into(),
            environment_names: vec![],
            timeout_seconds: 60,
            descriptor_revision: 1,
        })
        .collect::<Vec<_>>();
    let input = ResolvedEnvironmentIdentityInputV1 {
        schema_version: 1,
        canonicalization_version: 1,
        plan: CanonicalFinalVerificationPlanV1 {
            version: 1,
            profile_id: "reviewer-reuse".into(),
            profile_revision: 1,
            commands: commands.clone(),
            required_checks: required_checks.clone(),
            hermeticity: CanonicalHermeticityV1 {
                hermetic: true,
                reusable: true,
                network_access: false,
            },
        },
        input_manifest: manifest.clone(),
        image: ImmutableImageV1 {
            reference: "test-image".into(),
            digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
        },
        tool_probes:
            commands
                .iter()
                .map(|command| ToolProbeV1 {
                    tool: command.executable.clone(),
                    version: "test".into(),
                    executable_digest:
                        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .into(),
                    status: ToolProbeStatus::Passed,
                })
                .collect(),
        runner_version: "test-runner".into(),
        lockfile_digests: vec![],
        target: "test-target".into(),
        features: vec![],
        allowlisted_environment: Default::default(),
    };
    FinalVerificationResolvedMaterial {
        execution_request: FinalVerificationExecutionRequest {
            worktree,
            resolve_environment_identity: Arc::new(move || Ok(input.clone())),
            fingerprint_config: VerificationInputFingerprintConfig {
                base_ref: "main".into(),
                manifest,
                external_inputs: vec![],
            },
            tool_runtime: vec![],
            read_only_external_mounts: vec![],
            output_directories: vec![],
        },
        verify_source: VerifySource::Worker,
        required_checks,
        diff_fingerprint: "reviewer-reuse-audit-diff".into(),
    }
}

// ---------------------------------------------------------------------------
// Probe host callbacks: record every coordinator boundary crossing
// ---------------------------------------------------------------------------

/// Records every coordinator boundary crossing in order. Returning `None` from
/// `final_verification_outcome_for_test` keeps production consultation live so
/// the reviewer's `submit_review` traverses the real consult-or-run path.
struct ReviewerReuseCallbacks {
    material: FinalVerificationResolvedMaterial,
    events: Mutex<Vec<&'static str>>,
    lease_requests: Mutex<usize>,
    lease_acquisitions: Mutex<usize>,
    canonical_executions: Mutex<usize>,
    coordinator_entries: Mutex<usize>,
    #[allow(dead_code)]
    outcomes: Mutex<VecDeque<FinalVerificationRecordingOutcome>>,
}

impl ReviewerReuseCallbacks {
    fn new(material: FinalVerificationResolvedMaterial) -> Self {
        Self {
            material,
            events: Mutex::new(Vec::new()),
            lease_requests: Mutex::new(0),
            lease_acquisitions: Mutex::new(0),
            canonical_executions: Mutex::new(0),
            coordinator_entries: Mutex::new(0),
            outcomes: Mutex::new(VecDeque::new()),
        }
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

impl SlotHostCallbacks for ReviewerReuseCallbacks {
    fn final_verification_outcome_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationRecordingOutcome> {
        // Never short-circuit: the reviewer must traverse the real
        // consult-or-run path with zero canonical commands and zero lease
        // requests on a hit.
        *self.coordinator_entries.lock().unwrap() += 1;
        self.events
            .lock()
            .unwrap()
            .push("reviewer-coordinator-entered");
        None
    }

    fn final_verification_evidence_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<djinn_sandbox::final_verification_execution::FinalVerificationExecutionEvidence>
    {
        *self.canonical_executions.lock().unwrap() += 1;
        None
    }

    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        verify_run_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<FinalVerificationResolvedMaterial>, String>>
                + Send
                + 'a,
        >,
    > {
        self.events.lock().unwrap().push(match verify_run_id {
            "reuse-c0" => "consult-reuse-c0",
            "reuse-c1" => "consult-reuse-c1",
            _ => "writer-resolution",
        });
        let material = self.material.clone();
        Box::pin(async move { Ok(Some(material)) })
    }

    fn acquire_final_verification_lease<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Box<dyn crate::final_verification::FinalVerificationInvocationLease>,
                        String,
                    >,
                > + Send
                + 'a,
        >,
    > {
        *self.lease_requests.lock().unwrap() += 1;
        *self.lease_acquisitions.lock().unwrap() += 1;
        Box::pin(async { Err("lease must not be acquired for a reuse hit".into()) })
    }

    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<ResolvedMcpTools, String>> + Send + 'a>> {
        Box::pin(async { Err("not implemented in test".into()) })
    }
    fn render_prompt(
        &self,
        _role_name: &str,
        _task: &Task,
        _context_json: &serde_json::Value,
    ) -> String {
        String::new()
    }
    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }
    fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
        panic!("build_mcp_state not needed in reviewer reuse tests")
    }
    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                error: "not implemented".into(),
            })
        })
    }
    fn resolve_provider_credential<'a>(
        &'a self,
        _provider_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<crate::helpers::ProviderCredential, String>> + Send + 'a>>
    {
        Box::pin(async { Err("not implemented in test".into()) })
    }
    fn run_task_dispatch<'a>(
        &'a self,
        _task_id: String,
        _project_path: String,
        _model_id: String,
        _ctx: SlotContext,
        _kill: CancellationToken,
        _pause: CancellationToken,
        _resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
    fn touch_activity_rpc<'a>(
        &'a self,
        _task_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
    fn flush_session_tokens_rpc<'a>(
        &'a self,
        _session_id: String,
        _tokens_in: i64,
        _tokens_out: i64,
        _cache_read: i64,
        _cache_write: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn dummy_tool_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} },
        "concurrent_safe": false
    })
}

fn reviewer_conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a reviewer."));
    conversation.push(Message::user("Review the work."));
    conversation
}

/// A reviewer `submit_review` turn that approves the work.
fn submit_review_turn(id: &str, task_id: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: id.into(),
            name: "submit_review".into(),
            input: serde_json::json!({
                "task_id": task_id,
                "verdict": "approved",
                "acceptance_criteria": [
                    {"criterion": "write tests", "met": true},
                    {"criterion": "passing ci", "met": true}
                ],
                "feedback": null
            }),
        }),
        StreamEvent::Done,
    ]
}

// ---------------------------------------------------------------------------
// Criterion: reviewer reuses a post-authoring worker pass with zero canonical
// commands and zero lease requests/acquisitions; the setup-time pass is
// ineligible.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reviewer_reuses_post_authoring_worker_pass_with_zero_commands_and_zero_lease() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;

    // ── Initialize a real git worktree ───────────────────────────────────
    let tree = test_tempdir("reviewer-reuse-e2e-");
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Reviewer Reuse Test"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    // Setup-time state: the tree is committed before the worker edits.
    std::fs::write(tree.path().join("authored.txt"), "setup state").unwrap();
    for args in [
        vec!["add", "authored.txt"],
        vec!["commit", "-m", "setup state"],
        vec!["branch", "-M", "main"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }

    let material = reviewer_reuse_material(tree.path().to_path_buf());

    // ── Setup-time fingerprint: what a hypothetical setup pass would have ─
    let setup_fingerprint = match compute_verification_input_fingerprint_with_config(
        tree.path(),
        &material.execution_request.fingerprint_config,
    )
    .await
    .unwrap()
    {
        VerificationInputFingerprint::Available(digest) => digest.fingerprint,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("setup fingerprint unavailable: {reason}")
        }
    };

    // ── Worker edits after setup, changing the complete fingerprint ──────
    std::fs::write(tree.path().join("authored.txt"), "post-authoring edit").unwrap();
    for args in [
        vec!["add", "authored.txt"],
        vec!["commit", "-m", "post-authoring edit"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }

    let post_fingerprint = match compute_verification_input_fingerprint_with_config(
        tree.path(),
        &material.execution_request.fingerprint_config,
    )
    .await
    .unwrap()
    {
        VerificationInputFingerprint::Available(digest) => digest.fingerprint,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("post-authoring fingerprint unavailable: {reason}")
        }
    };
    assert_ne!(
        setup_fingerprint, post_fingerprint,
        "the post-authoring fingerprint must reflect the edit (setup pass must be ineligible)"
    );

    let identity = EnvironmentIdentityV1::derive(
        (material.execution_request.resolve_environment_identity)().unwrap(),
    )
    .unwrap();

    // ── Create the worker's task run and record its post-authoring pass ──
    let worker_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &worker_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("closed"),
            workspace_path: Some(tree.path().to_str().unwrap()),
            mirror_ref: None,
        })
        .await
        .unwrap();

    // Enable reuse so the reviewer consultation is live.
    SettingsRepository::new(db.clone(), crate::test_helpers::test_events())
        .set(
            &format!("project.{}.verify_run_reuse_enabled", project.id),
            "true",
        )
        .await
        .unwrap();

    let persisted_id = "persisted-reviewer-reuse-hit";
    let completed_at = "2099-03-03T04:05:06Z";
    let ordered_commands = serde_json::json!([
        {"descriptor_id": "format", "result": "pass", "passed": true},
        {"descriptor_id": "slot-clippy", "result": "pass", "passed": true}
    ]);
    let covered_checks = serde_json::json!(["format", "slot-clippy"]);
    let commands = [
        RequiredFinalVerificationCommand {
            descriptor_id: "format",
        },
        RequiredFinalVerificationCommand {
            descriptor_id: "slot-clippy",
        },
    ];
    let identity_json = serde_json::from_str(&identity.canonical_json).unwrap();
    VerifyRunRepository::new(db.clone())
        .record_eligible_final_verification_pass(RecordEligibleFinalVerificationPassParams {
            id: persisted_id,
            task_run_id: &worker_run_id,
            verify_source: "worker",
            verify_run_id: "seeded-reviewer-reuse-run",
            verification_attempt_id: "seeded-reviewer-reuse-attempt",
            required_commands: &commands,
            ordered_commands: &ordered_commands,
            covered_checks: &covered_checks,
            required_checks: &material.required_checks,
            verification_input_fingerprint: &post_fingerprint,
            manifest_version: "manifest-v1",
            environment_identity_json: &identity_json,
            environment_identity_digest: &identity.digest,
            environment_identity_version: "identity-v1",
            completed_at,
            diff_fingerprint: &material.diff_fingerprint,
        })
        .await
        .unwrap();

    // ── Create the reviewer's task run on the same task/worktree ────────
    let reviewer_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &reviewer_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some(tree.path().to_str().unwrap()),
            mirror_ref: None,
        })
        .await
        .unwrap();

    let callbacks = Arc::new(ReviewerReuseCallbacks::new(material));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    let session = SessionRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone())
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "synthetic/test-model",
            agent_type: "reviewer",
            metadata_json: None,
            task_run_id: Some(&reviewer_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let provider = FakeProvider::script(vec![submit_review_turn("submit-review-reused", &task.id)]);
    let mut conversation = reviewer_conversation();
    let cancel = CancellationToken::new();
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[dummy_tool_schema("submit_review")],
            task_id: &task.id,
            task_short_id: "t1",
            session_id: &session.id,
            project_path: tree.path().to_str().unwrap(),
            worktree_path: tree.path(),
            role_name: "reviewer",
            finalize_tool_names: &["submit_review", "request_planner"],
            context_window: 10_000,
            model_id: "synthetic/test-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conversation,
        false,
    )
    .await;

    // ── The reviewer's submit_review completed successfully ─────────────
    assert!(result.is_ok());
    assert_eq!(
        output.finalize_tool_name.as_deref(),
        Some("submit_review"),
        "the reviewer session completed via submit_review"
    );

    // ── The reviewer hit: coordinator entered, consulted C0/C1, no writer ─
    let events = callbacks.events();
    assert_eq!(
        events,
        vec![
            "reviewer-coordinator-entered",
            "consult-reuse-c0",
            "consult-reuse-c1"
        ],
        "the reviewer must consult the same cache and never enter the writer path"
    );

    // ── Zero canonical commands and zero lease requests/acquisitions ────
    assert_eq!(
        *callbacks.canonical_executions.lock().unwrap(),
        0,
        "reviewer hit must execute no canonical verification command"
    );
    assert_eq!(
        *callbacks.lease_requests.lock().unwrap(),
        0,
        "reviewer hit must never request a verification lease"
    );
    assert_eq!(
        *callbacks.lease_acquisitions.lock().unwrap(),
        0,
        "reviewer hit must never acquire a verification lease"
    );

    // ── The injected evidence matches the worker's post-authoring pass ──
    let intent = output
        .completion_intent
        .expect("reviewer hit must carry completion intent with injected evidence");
    let evidence = intent
        .final_verification_evidence
        .as_ref()
        .expect("reviewer hit must inject the persisted run evidence");
    assert_eq!(evidence.persisted_run_id, persisted_id);
    assert_eq!(evidence.completed_at, completed_at);
    assert_eq!(evidence.ordered_commands, ordered_commands);
    assert_eq!(evidence.covered_checks, covered_checks);
    assert_eq!(evidence.required_checks, vec!["format", "slot-clippy"]);
    assert_eq!(evidence.verification_input_fingerprint, post_fingerprint);
    assert_eq!(evidence.manifest_version, "manifest-v1");
    assert_eq!(evidence.environment_identity_digest, identity.digest);

    // ── The setup-time fingerprint was NOT reused (different fingerprint) ─
    assert_ne!(
        evidence.verification_input_fingerprint, setup_fingerprint,
        "the reviewer must not reuse a setup-time pass"
    );

    // ── The reviewer's verdict payload survived unchanged ───────────────
    let payload = output.finalize_payload.as_ref().unwrap();
    assert_eq!(payload["verdict"], "approved");
}

// ---------------------------------------------------------------------------
// Criterion: reviewer reuse does NOT intercept arbitrary reviewer shell
// commands — the reviewer's tools (shell, etc.) dispatch normally, and only
// the `submit_review` finalize boundary consults the reuse cache.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reviewer_arbitrary_shell_command_is_not_intercepted_by_reuse() {
    use crate::test_helpers::{ConfigurableToolDispatcher, ToolHandlerFn};
    use std::collections::HashMap;

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let tree = test_tempdir("reviewer-shell-no-intercept-");
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Reviewer Shell Test"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    std::fs::write(tree.path().join("authored.txt"), "authored").unwrap();
    for args in [
        vec!["add", "authored.txt"],
        vec!["commit", "-m", "init"],
        vec!["branch", "-M", "main"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    let material = reviewer_reuse_material(tree.path().to_path_buf());
    let run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some(tree.path().to_str().unwrap()),
            mirror_ref: None,
        })
        .await
        .unwrap();
    SettingsRepository::new(db.clone(), crate::test_helpers::test_events())
        .set(
            &format!("project.{}.verify_run_reuse_enabled", project.id),
            "true",
        )
        .await
        .unwrap();

    // Seed a reusable post-authoring pass so the reviewer's submit_review hits
    // the reuse cache. The purpose of this test is to prove the shell command
    // is NOT intercepted — the reviewer's verification still works normally.
    let post_fingerprint = match compute_verification_input_fingerprint_with_config(
        tree.path(),
        &material.execution_request.fingerprint_config,
    )
    .await
    .unwrap()
    {
        VerificationInputFingerprint::Available(digest) => digest.fingerprint,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("fingerprint unavailable: {reason}")
        }
    };
    let identity = EnvironmentIdentityV1::derive(
        (material.execution_request.resolve_environment_identity)().unwrap(),
    )
    .unwrap();
    let identity_json = serde_json::from_str(&identity.canonical_json).unwrap();
    let commands = [
        RequiredFinalVerificationCommand {
            descriptor_id: "format",
        },
        RequiredFinalVerificationCommand {
            descriptor_id: "slot-clippy",
        },
    ];
    let ordered_commands = serde_json::json!([
        {"descriptor_id": "format", "result": "pass", "passed": true},
        {"descriptor_id": "slot-clippy", "result": "pass", "passed": true}
    ]);
    let covered_checks = serde_json::json!(["format", "slot-clippy"]);
    VerifyRunRepository::new(db.clone())
        .record_eligible_final_verification_pass(RecordEligibleFinalVerificationPassParams {
            id: "persisted-reviewer-shell-reuse",
            task_run_id: &run_id,
            verify_source: "worker",
            verify_run_id: "seeded-shell-reuse-run",
            verification_attempt_id: "seeded-shell-reuse-attempt",
            required_commands: &commands,
            ordered_commands: &ordered_commands,
            covered_checks: &covered_checks,
            required_checks: &material.required_checks,
            verification_input_fingerprint: &post_fingerprint,
            manifest_version: "manifest-v1",
            environment_identity_json: &identity_json,
            environment_identity_digest: &identity.digest,
            environment_identity_version: "identity-v1",
            completed_at: "2099-04-04T05:06:07Z",
            diff_fingerprint: &material.diff_fingerprint,
        })
        .await
        .unwrap();

    let callbacks = Arc::new(ReviewerReuseCallbacks::new(material));
    // Build a SlotContext with a ConfigurableToolDispatcher that handles the
    // arbitrary `shell` command (not a finalize tool), so the reply loop can
    // dispatch it normally without consulting the reuse cache.
    let shell_handler: ToolHandlerFn =
        |_args| Ok(serde_json::json!({"stdout": "hello", "exit_code": 0}));
    let mut handlers: HashMap<String, ToolHandlerFn> = HashMap::new();
    handlers.insert("shell".into(), shell_handler);
    let dispatcher: Arc<dyn crate::host::SlotToolDispatcher> =
        Arc::new(ConfigurableToolDispatcher::new(vec![], handlers));
    let event_bus = crate::test_helpers::test_events();
    let slot_ctx = SlotContext {
        db: db.clone(),
        event_bus: event_bus.clone(),
        catalog: djinn_provider::catalog::CatalogService::new(),
        health_tracker: djinn_provider::catalog::HealthTracker::default(),
        background_work_tasks: Arc::new(Mutex::new(std::collections::HashSet::new())),
        active_tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
        default_project_id: None,
        working_root: None,
        coordinator_trigger: None,
        runtime_ops: None,
        repo_graph_ops: None,
        clock: Arc::new(djinn_core::clock::SystemClock::new()),
        callbacks: callbacks.clone(),
        tool_dispatcher: Some(dispatcher),
        compaction_cs: crate::reply_loop::CompactionCriticalSection::new(),
    };
    let session = SessionRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone())
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "synthetic/test-model",
            agent_type: "reviewer",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // The reviewer runs an arbitrary shell command (not a finalize tool) and
    // then calls submit_review. The shell command must dispatch normally
    // without consulting the reuse cache; only submit_review triggers the
    // consult-or-run boundary.
    let shell_turn = vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "shell-1".into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "echo hello"}),
        }),
        StreamEvent::Done,
    ];
    let review_turn = submit_review_turn("submit-review-shell", &task.id);
    let provider = FakeProvider::script(vec![shell_turn, review_turn]);
    let mut conversation = reviewer_conversation();
    let cancel = CancellationToken::new();
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[
                dummy_tool_schema("shell"),
                dummy_tool_schema("submit_review"),
            ],
            task_id: &task.id,
            task_short_id: "t1",
            session_id: &session.id,
            project_path: tree.path().to_str().unwrap(),
            worktree_path: tree.path(),
            role_name: "reviewer",
            finalize_tool_names: &["submit_review", "request_planner"],
            context_window: 10_000,
            model_id: "synthetic/test-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conversation,
        false,
    )
    .await;

    assert!(result.is_ok());
    // The shell command completed without consulting the reuse cache; the
    // coordinator only entered when submit_review was called.
    assert_eq!(
        output.finalize_tool_name.as_deref(),
        Some("submit_review"),
        "the reviewer session completed via submit_review after shell command"
    );
    assert_eq!(
        *callbacks.coordinator_entries.lock().unwrap(),
        1,
        "the coordinator must enter exactly once (for submit_review), not for the shell command"
    );
}
