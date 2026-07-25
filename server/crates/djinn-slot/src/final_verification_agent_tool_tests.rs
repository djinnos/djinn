//! End-to-end coverage for the agent-facing `run_verification` coordinator
//! client (epic hexh).
//!
//! Each test drives the SAME authoritative consult-or-run entry the
//! completion-intent path uses, against a real git worktree and a real
//! `verify_runs` table, and asserts the deliverable invariants:
//!   * a worker tool MISS runs and records (a durable pass row is written);
//!   * an immediately following `submit_work` on the unchanged tree only
//!     consults and finalizes (reuse, no lease);
//!   * a fresh-hit tool call emits a `hit` outcome WITHOUT acquiring a lease;
//!   * a reviewer tool call on a CHANGED tree misses and runs;
//!   * a rate-limited tool call is typed and NEVER consumes a lease.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::final_verification::{
    AgentRunVerificationOutcome, FinalVerificationCoordinatorRequest,
    FinalVerificationInvocationLease, FinalVerificationPreLeaseGate, FinalVerificationRateLimited,
    FinalVerificationResolvedMaterial, FinalVerificationRunPermit,
    coordinate_final_verification_for_agent, verify_completion_intent,
};
use crate::host::{ResolvedMcpTools, SlotContext, SlotHostCallbacks};
use crate::output_parser::{CompletionIntent, FinalVerificationDisposition};
use crate::test_helpers::{
    agent_context_from_db_with_callbacks, create_test_db, create_test_epic, create_test_project,
    create_test_task,
};
use djinn_core::canonical_verify::{
    CanonicalCommandDescriptorV1, CanonicalFinalVerificationPlanV1, CanonicalHermeticityV1,
    EnvironmentIdentityV1, ImmutableImageV1, ResolvedEnvironmentIdentityInputV1,
    ResolvedVerificationSelectionV1, ToolProbeStatus, ToolProbeV1, VerificationInputManifestV1,
};
use djinn_core::models::{Task, VerifySource};
use djinn_db::repositories::settings::SettingsRepository;
use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
use djinn_db::repositories::verify_run::VerifyRunRepository;
use djinn_git::{
    VerificationInputDigestV1, VerificationInputFingerprint, VerificationInputFingerprintConfig,
    compute_verification_input_fingerprint_with_config, run_git_command_in,
};
use djinn_sandbox::final_verification_execution::{
    FinalVerificationCommandEvidence, FinalVerificationExecutionEvidence,
    FinalVerificationExecutionRequest, FinalVerificationIneligibilityReason,
};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Deterministic material builder (mirrors the reply-loop reuse harness).
// ---------------------------------------------------------------------------

fn agent_tool_material(worktree: std::path::PathBuf) -> FinalVerificationResolvedMaterial {
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
            profile_id: "agent-tool".into(),
            profile_revision: 1,
            commands: commands.clone(),
            required_checks: required_checks.clone(),
            hermeticity: CanonicalHermeticityV1 {
                hermetic: true,
                reusable: true,
                network_access: false,
            },
        },
        selection: ResolvedVerificationSelectionV1::legacy_flat_plan(),
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
        services: vec![],
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
            catalog_loopback_endpoints: vec![],
            service_provisioners: vec![],
        },
        verify_source: VerifySource::Worker,
        required_checks,
        diff_fingerprint: "agent-tool-audit-diff".into(),
    }
}

async fn current_fingerprint(material: &FinalVerificationResolvedMaterial) -> String {
    match compute_verification_input_fingerprint_with_config(
        &material.execution_request.worktree,
        &material.execution_request.fingerprint_config,
    )
    .await
    .unwrap()
    {
        VerificationInputFingerprint::Available(digest) => digest.fingerprint,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("fingerprint unavailable: {reason}")
        }
    }
}

/// Build eligible execution evidence whose durable inputs match the current tree
/// and material, so a run records a row that later consults can reuse.
fn eligible_evidence(
    material: &FinalVerificationResolvedMaterial,
    fingerprint: &str,
) -> FinalVerificationExecutionEvidence {
    let identity = EnvironmentIdentityV1::derive(
        (material.execution_request.resolve_environment_identity)().unwrap(),
    )
    .unwrap();
    let digest = VerificationInputDigestV1 {
        version: 1,
        fingerprint: fingerprint.to_owned(),
        canonical_stream_len: 0,
        merge_base: None,
        head: "head".into(),
        tracked_entry_count: 0,
        extra_entry_count: 0,
    };
    let commands = material
        .required_checks
        .iter()
        .enumerate()
        .map(|(idx, check_id)| FinalVerificationCommandEvidence {
            descriptor: CanonicalCommandDescriptorV1 {
                check_id: check_id.clone(),
                executable: format!("{check_id}-tool"),
                argv: vec![check_id.clone()],
                working_directory: ".".into(),
                environment_names: vec![],
                timeout_seconds: 60,
                descriptor_revision: 1,
            },
            started_at_unix_millis: 1_000 + idx as u128,
            completed_at_unix_millis: 1_050 + idx as u128,
            exit_code: Some(0),
            timed_out: false,
        })
        .collect();
    FinalVerificationExecutionEvidence {
        manifest_version: 1,
        pre_environment_identity: Some(identity.clone()),
        post_environment_identity: Some(identity),
        fingerprint_f0: Some(digest.clone()),
        fingerprint_f1: Some(digest),
        commands,
        eligibility_reason: None,
    }
}

/// Build execution evidence for a run where one required check fails.
fn failing_evidence(
    material: &FinalVerificationResolvedMaterial,
    fingerprint: &str,
) -> FinalVerificationExecutionEvidence {
    let mut evidence = eligible_evidence(material, fingerprint);
    if let Some(last) = evidence.commands.last_mut() {
        last.exit_code = Some(1);
    }
    let failing_check = material.required_checks.last().cloned().unwrap_or_default();
    evidence.eligibility_reason = Some(FinalVerificationIneligibilityReason::CommandFailed {
        check_id: failing_check,
        exit_code: Some(1),
    });
    evidence
}

// ---------------------------------------------------------------------------
// Probe callbacks: count every lease request/acquisition and canonical run.
// ---------------------------------------------------------------------------

struct NoopLease;
impl FinalVerificationInvocationLease for NoopLease {
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

struct AgentToolCallbacks {
    material: FinalVerificationResolvedMaterial,
    injected_evidence: Mutex<Option<FinalVerificationExecutionEvidence>>,
    lease_requests: Mutex<usize>,
    canonical_executions: Mutex<usize>,
}

impl AgentToolCallbacks {
    fn new(
        material: FinalVerificationResolvedMaterial,
        evidence: FinalVerificationExecutionEvidence,
    ) -> Self {
        Self {
            material,
            injected_evidence: Mutex::new(Some(evidence)),
            lease_requests: Mutex::new(0),
            canonical_executions: Mutex::new(0),
        }
    }
    fn lease_requests(&self) -> usize {
        *self.lease_requests.lock().unwrap()
    }
    fn canonical_executions(&self) -> usize {
        *self.canonical_executions.lock().unwrap()
    }
}

impl SlotHostCallbacks for AgentToolCallbacks {
    fn final_verification_outcome_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<crate::final_verification::FinalVerificationRecordingOutcome> {
        // Never short-circuit: exercise the real consult-or-run path.
        None
    }
    fn final_verification_evidence_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationExecutionEvidence> {
        *self.canonical_executions.lock().unwrap() += 1;
        self.injected_evidence.lock().unwrap().clone()
    }
    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _attempt: &'a str,
        _verify_run: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<FinalVerificationResolvedMaterial>, String>>
                + Send
                + 'a,
        >,
    > {
        let material = self.material.clone();
        Box::pin(async move { Ok(Some(material)) })
    }
    fn acquire_final_verification_lease<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _attempt: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Box<dyn FinalVerificationInvocationLease>, String>>
                + Send
                + 'a,
        >,
    > {
        *self.lease_requests.lock().unwrap() += 1;
        Box::pin(async { Ok(Box::new(NoopLease) as Box<dyn FinalVerificationInvocationLease>) })
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
        panic!("build_mcp_state not needed in agent tool tests")
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
// Test gates.
// ---------------------------------------------------------------------------

/// Always allows; counts how many times the pre-lease gate was consulted.
struct CountingAllowGate {
    acquisitions: Arc<Mutex<usize>>,
}
impl FinalVerificationPreLeaseGate for CountingAllowGate {
    fn acquire(&mut self) -> Result<FinalVerificationRunPermit, FinalVerificationRateLimited> {
        *self.acquisitions.lock().unwrap() += 1;
        Ok(FinalVerificationRunPermit::new(Box::new(())))
    }
}

/// Always denies with a typed hourly rate-limit outcome.
struct DenyGate;
impl FinalVerificationPreLeaseGate for DenyGate {
    fn acquire(&mut self) -> Result<FinalVerificationRunPermit, FinalVerificationRateLimited> {
        Err(FinalVerificationRateLimited {
            scope: "hourly".to_owned(),
            detail: "test denial".to_owned(),
            retry_after_seconds: Some(42),
        })
    }
}

async fn init_worktree(prefix: &str) -> tempfile::TempDir {
    let tree = crate::test_helpers::test_tempdir(prefix);
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Agent Tool Test"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    std::fs::write(tree.path().join("authored.txt"), "initial state").unwrap();
    for args in [
        vec!["add", "authored.txt"],
        vec!["commit", "-m", "initial"],
        vec!["branch", "-M", "main"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    tree
}

async fn enable_reuse(ctx: &SlotContext, project_id: &str) {
    SettingsRepository::new(ctx.db.clone(), ctx.event_bus.clone())
        .set(
            &format!("project.{project_id}.verify_run_reuse_enabled"),
            "true",
        )
        .await
        .unwrap();
}

async fn create_running_task_run(
    ctx: &SlotContext,
    project_id: &str,
    task_id: &str,
    tree: &std::path::Path,
) -> String {
    let run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(ctx.db.clone())
        .create(CreateTaskRunParams {
            id: &run_id,
            project_id,
            task_id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some(tree.to_str().unwrap()),
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    run_id
}

// ---------------------------------------------------------------------------
// Deliverable 1 + 2 + 5: worker MISS runs and records; the immediately
// following submit_work on the unchanged tree only consults (reuse, no lease);
// a fresh-hit tool call emits `hit` WITHOUT leasing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_tool_miss_runs_and_records_then_unchanged_tree_reuses() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let tree = init_worktree("agent-tool-miss-").await;
    let material = agent_tool_material(tree.path().to_path_buf());
    let fingerprint = current_fingerprint(&material).await;
    let evidence = eligible_evidence(&material, &fingerprint);

    let callbacks = Arc::new(AgentToolCallbacks::new(material.clone(), evidence));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    enable_reuse(&slot_ctx, &project.id).await;
    let run_id = create_running_task_run(&slot_ctx, &project.id, &task.id, tree.path()).await;

    // ── First tool call: a MISS runs and records a durable pass row. ────────
    let acquisitions = Arc::new(Mutex::new(0usize));
    let mut gate = CountingAllowGate {
        acquisitions: Arc::clone(&acquisitions),
    };
    let outcome = coordinate_final_verification_for_agent(
        FinalVerificationCoordinatorRequest {
            task_id: task.id.clone(),
            task_run_id: run_id.clone(),
            cancellation: CancellationToken::new(),
        },
        &slot_ctx,
        &mut gate,
    )
    .await;
    match &outcome {
        AgentRunVerificationOutcome::RanPass { checks, .. } => {
            assert_eq!(
                checks.len(),
                2,
                "per-check pass results for each required check"
            );
            assert!(checks.iter().all(|check| check.passed));
        }
        other => panic!("expected RanPass on a miss, got {other:?}"),
    }
    assert_eq!(
        callbacks.lease_requests(),
        1,
        "a miss acquires exactly one lease"
    );
    assert_eq!(
        callbacks.canonical_executions(),
        1,
        "a miss executes exactly once"
    );
    assert_eq!(
        *acquisitions.lock().unwrap(),
        1,
        "a miss consults the pre-lease gate once"
    );

    // A durable passing row now exists for the task run.
    let recorded = VerifyRunRepository::new(slot_ctx.db.clone())
        .latest_compatible_passing_final_verification(
            &task.id,
            &fingerprint,
            "manifest-v1",
            "identity-v1",
            &material.required_checks,
        )
        .await
        .unwrap();
    assert!(
        recorded.is_some(),
        "the miss recorded a reusable passing row"
    );

    // ── Second tool call on the unchanged tree: a fresh HIT, no lease, no
    //    gate consultation (hits are free). ────────────────────────────────
    let mut hit_gate = CountingAllowGate {
        acquisitions: Arc::clone(&acquisitions),
    };
    let hit = coordinate_final_verification_for_agent(
        FinalVerificationCoordinatorRequest {
            task_id: task.id.clone(),
            task_run_id: run_id.clone(),
            cancellation: CancellationToken::new(),
        },
        &slot_ctx,
        &mut hit_gate,
    )
    .await;
    assert!(
        matches!(hit, AgentRunVerificationOutcome::Hit { .. }),
        "an unchanged tree is a fresh hit, got {hit:?}"
    );
    assert_eq!(
        callbacks.lease_requests(),
        1,
        "a hit never requests a lease"
    );
    assert_eq!(
        *acquisitions.lock().unwrap(),
        1,
        "a hit never consults the pre-lease gate (hits are free)"
    );

    // ── submit_work on the unchanged tree only consults and finalizes. ──────
    let mut intent = CompletionIntent {
        finalize_payload: serde_json::json!({"task_id": task.id}),
        tool_use_id: "submit-work-unchanged".into(),
        final_verification_evidence: None,
        final_verification_disposition: FinalVerificationDisposition::Pending,
    };
    let completion = verify_completion_intent(
        &mut intent,
        &task.id,
        Some(&run_id),
        CancellationToken::new(),
        &slot_ctx,
        "submit_work",
    )
    .await
    .expect("submit_work reuses the recorded pass");
    assert!(completion.is_some(), "submit_work carries reused evidence");
    assert_eq!(
        callbacks.lease_requests(),
        1,
        "submit_work reuse acquires no new lease"
    );
    assert_eq!(
        callbacks.canonical_executions(),
        1,
        "submit_work reuse executes no canonical command"
    );
}

// ---------------------------------------------------------------------------
// Deliverable 4: a reviewer tool call on a CHANGED tree misses and runs.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reviewer_tool_on_changed_tree_misses_and_runs() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let tree = init_worktree("agent-tool-changed-").await;
    let material = agent_tool_material(tree.path().to_path_buf());
    let first_fp = current_fingerprint(&material).await;

    let callbacks = Arc::new(AgentToolCallbacks::new(
        material.clone(),
        eligible_evidence(&material, &first_fp),
    ));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    enable_reuse(&slot_ctx, &project.id).await;
    let run_id = create_running_task_run(&slot_ctx, &project.id, &task.id, tree.path()).await;

    // First run records a pass for the current tree.
    let acquisitions = Arc::new(Mutex::new(0usize));
    let mut gate = CountingAllowGate {
        acquisitions: Arc::clone(&acquisitions),
    };
    let first = coordinate_final_verification_for_agent(
        FinalVerificationCoordinatorRequest {
            task_id: task.id.clone(),
            task_run_id: run_id.clone(),
            cancellation: CancellationToken::new(),
        },
        &slot_ctx,
        &mut gate,
    )
    .await;
    assert!(matches!(first, AgentRunVerificationOutcome::RanPass { .. }));
    assert_eq!(callbacks.lease_requests(), 1);

    // ── The reviewer edits the tree: the fingerprint changes, so the prior
    //    pass is not reusable. ───────────────────────────────────────────────
    std::fs::write(tree.path().join("authored.txt"), "reviewer edit").unwrap();
    for args in [vec!["add", "authored.txt"], vec!["commit", "-m", "edit"]] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    let changed_fp = current_fingerprint(&material).await;
    assert_ne!(
        first_fp, changed_fp,
        "the edit changed the whole-tree fingerprint"
    );
    // Refresh the injected evidence so the new run records the changed tree.
    *callbacks.injected_evidence.lock().unwrap() = Some(eligible_evidence(&material, &changed_fp));

    let changed = coordinate_final_verification_for_agent(
        FinalVerificationCoordinatorRequest {
            task_id: task.id.clone(),
            task_run_id: run_id.clone(),
            cancellation: CancellationToken::new(),
        },
        &slot_ctx,
        &mut gate,
    )
    .await;
    assert!(
        matches!(changed, AgentRunVerificationOutcome::RanPass { .. }),
        "a changed tree misses and runs, got {changed:?}"
    );
    assert_eq!(
        callbacks.lease_requests(),
        2,
        "the changed-tree run acquires a second lease (it was a miss)"
    );
    assert_eq!(
        *acquisitions.lock().unwrap(),
        2,
        "both misses consulted the gate"
    );
}

// ---------------------------------------------------------------------------
// Deliverable: a rate-limited outcome is typed and NEVER consumes a lease.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limited_tool_call_never_acquires_a_lease() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let tree = init_worktree("agent-tool-ratelimit-").await;
    let material = agent_tool_material(tree.path().to_path_buf());
    let fingerprint = current_fingerprint(&material).await;

    let callbacks = Arc::new(AgentToolCallbacks::new(
        material.clone(),
        eligible_evidence(&material, &fingerprint),
    ));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    enable_reuse(&slot_ctx, &project.id).await;
    let run_id = create_running_task_run(&slot_ctx, &project.id, &task.id, tree.path()).await;

    // No prior pass exists, so the consult misses and reaches the gate — which
    // denies. No lease may be acquired and no command executed.
    let mut gate = DenyGate;
    let outcome = coordinate_final_verification_for_agent(
        FinalVerificationCoordinatorRequest {
            task_id: task.id.clone(),
            task_run_id: run_id.clone(),
            cancellation: CancellationToken::new(),
        },
        &slot_ctx,
        &mut gate,
    )
    .await;
    match outcome {
        AgentRunVerificationOutcome::RateLimited {
            scope,
            retry_after_seconds,
            ..
        } => {
            assert_eq!(scope, "hourly");
            assert_eq!(retry_after_seconds, Some(42));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert_eq!(
        callbacks.lease_requests(),
        0,
        "a rate-limited outcome must never acquire a lease"
    );
    assert_eq!(
        callbacks.canonical_executions(),
        0,
        "a rate-limited outcome must never execute a command"
    );
}

// ---------------------------------------------------------------------------
// Deliverable: a real check failure is a typed RanFail (not infra Error) and
// writes no passing row.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failing_check_is_typed_ran_fail_and_writes_no_row() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let tree = init_worktree("agent-tool-fail-").await;
    let material = agent_tool_material(tree.path().to_path_buf());
    let fingerprint = current_fingerprint(&material).await;

    let callbacks = Arc::new(AgentToolCallbacks::new(
        material.clone(),
        failing_evidence(&material, &fingerprint),
    ));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    enable_reuse(&slot_ctx, &project.id).await;
    let run_id = create_running_task_run(&slot_ctx, &project.id, &task.id, tree.path()).await;

    let acquisitions = Arc::new(Mutex::new(0usize));
    let mut gate = CountingAllowGate {
        acquisitions: Arc::clone(&acquisitions),
    };
    let outcome = coordinate_final_verification_for_agent(
        FinalVerificationCoordinatorRequest {
            task_id: task.id.clone(),
            task_run_id: run_id.clone(),
            cancellation: CancellationToken::new(),
        },
        &slot_ctx,
        &mut gate,
    )
    .await;
    match &outcome {
        AgentRunVerificationOutcome::RanFail { checks, .. } => {
            assert!(
                checks.iter().any(|check| !check.passed),
                "a failing run returns a per-check failure result"
            );
        }
        other => panic!("expected RanFail on a failing check, got {other:?}"),
    }
    assert_eq!(
        callbacks.lease_requests(),
        1,
        "a failing run still ran (one lease)"
    );

    // No passing row was written.
    let recorded = VerifyRunRepository::new(slot_ctx.db.clone())
        .latest_compatible_passing_final_verification(
            &task.id,
            &fingerprint,
            "manifest-v1",
            "identity-v1",
            &material.required_checks,
        )
        .await
        .unwrap();
    assert!(recorded.is_none(), "a failing run writes no passing row");
}
