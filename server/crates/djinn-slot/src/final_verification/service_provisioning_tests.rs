//! Coordinator-boundary regressions for catalog-service provisioning.
//!
//! These prove the authoritative recording coordinator treats a service
//! lifecycle failure as a typed infrastructure-ineligible outcome distinct from
//! a command/check failure, that no such outcome can reach pass persistence, and
//! that provisioning telemetry stays bounded with identifiers confined to the
//! structured audit event. A Postgres-backed reuse case proves a matching hit
//! returns the stored pass without provisioning or executing anything.

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use djinn_core::canonical_verify::EnvironmentIdentityV1;
use djinn_core::models::Task;
use djinn_db::repositories::settings::SettingsRepository;
use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
use djinn_db::repositories::verify_run::{
    RecordEligibleFinalVerificationPassParams, RequiredFinalVerificationCommand,
    VerifyRunRepository,
};
use djinn_git::{
    VerificationInputFingerprint, compute_verification_input_fingerprint, run_git_command_in,
};
use djinn_sandbox::final_verification_execution::{
    FinalVerificationCommandEvidence, FinalVerificationExecutionEvidence,
    FinalVerificationIneligibilityReason,
};
use djinn_sandbox::service_provisioning::{
    CatalogServiceProvisioner, ServiceLease, ServiceProvisioningCode, ServiceProvisioningError,
    ServiceProvisioningPhase,
};
use tokio_util::sync::CancellationToken;

use super::provisioning_gate::classify_service_type;
use super::*;
use crate::host::{ResolvedMcpTools, SlotHostCallbacks};
use crate::reply_loop_completion_intent_tests::reuse_material;

// ---------------------------------------------------------------------------
// Provisioner + probe fixtures
// ---------------------------------------------------------------------------

/// Counts every lifecycle call so tests can prove a reuse hit provisions
/// nothing and a serviced run provisions exactly once.
#[derive(Default)]
struct ProvisionCounts {
    create: AtomicUsize,
    ready: AtomicUsize,
    delete: AtomicUsize,
}

struct TrackingProvisioner {
    preset: String,
    counts: Arc<ProvisionCounts>,
}

impl CatalogServiceProvisioner for TrackingProvisioner {
    fn preset_id(&self) -> &str {
        &self.preset
    }
    fn create<'a>(
        &'a self,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ServiceLease, ServiceProvisioningError>> + Send + 'a>>
    {
        self.counts.create.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(ServiceLease {
                lease_id: "lease".into(),
                environment: Default::default(),
            })
        })
    }
    fn ready<'a>(
        &'a self,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>> {
        self.counts.ready.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn delete<'a>(
        &'a self,
        _: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), ServiceProvisioningError>> + Send + 'a>> {
        self.counts.delete.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

fn tracking_material(
    preset: &str,
    checks: &[&str],
) -> (FinalVerificationResolvedMaterial, Arc<ProvisionCounts>) {
    let counts = Arc::new(ProvisionCounts::default());
    let mut material = bare_material(checks);
    material.execution_request.service_provisioners = vec![Arc::new(TrackingProvisioner {
        preset: preset.to_owned(),
        counts: Arc::clone(&counts),
    })];
    (material, counts)
}

/// Material whose resolver panics (injected evidence only) but which can carry
/// service provisioners. Mirrors `recording_tests::material`.
fn bare_material(checks: &[&str]) -> FinalVerificationResolvedMaterial {
    FinalVerificationResolvedMaterial {
        execution_request: FinalVerificationExecutionRequest {
            worktree: std::path::PathBuf::new(),
            resolve_environment_identity: Arc::new(|| panic!("injected evidence only")),
            fingerprint_config: djinn_git::VerificationInputFingerprintConfig::default(),
            tool_runtime: vec![],
            read_only_external_mounts: vec![],
            output_directories: vec![],
            catalog_loopback_endpoints: vec![],
            service_provisioners: vec![],
        },
        verify_source: djinn_core::models::VerifySource::Worker,
        required_checks: checks.iter().map(|c| (*c).to_owned()).collect(),
        diff_fingerprint: "audit-diff".into(),
    }
}

fn command(id: &str) -> FinalVerificationCommandEvidence {
    FinalVerificationCommandEvidence {
        descriptor: djinn_core::canonical_verify::CanonicalCommandDescriptorV1 {
            check_id: id.into(),
            executable: "hermetic-tool".into(),
            argv: vec![id.into()],
            working_directory: ".".into(),
            environment_names: vec![],
            timeout_seconds: 60,
            descriptor_revision: 1,
        },
        started_at_unix_millis: 10,
        completed_at_unix_millis: 11,
        exit_code: Some(0),
        timed_out: false,
    }
}

fn fingerprint(value: &str) -> djinn_git::VerificationInputDigestV1 {
    djinn_git::VerificationInputDigestV1 {
        version: 1,
        fingerprint: value.into(),
        canonical_stream_len: value.len() as u64,
        merge_base: Some("base".into()),
        head: "head".into(),
        tracked_entry_count: 1,
        extra_entry_count: 0,
    }
}

fn identity(value: &str) -> EnvironmentIdentityV1 {
    EnvironmentIdentityV1 {
        schema_version: 1,
        canonicalization_version: 1,
        canonical_json: format!(r#"{{"environment":"{value}"}}"#),
        digest: format!("identity-{value}"),
    }
}

fn passing_evidence(value: &str) -> FinalVerificationExecutionEvidence {
    FinalVerificationExecutionEvidence {
        manifest_version: 1,
        pre_environment_identity: Some(identity("stable")),
        post_environment_identity: Some(identity("stable")),
        fingerprint_f0: Some(fingerprint(value)),
        fingerprint_f1: Some(fingerprint(value)),
        commands: vec![command("lint"), command("test")],
        eligibility_reason: None,
    }
}

/// Evidence shaped exactly like the sandbox `service_error_evidence`: no
/// fingerprint, identity, or command, only a typed provisioning ineligibility.
fn provisioning_failure_evidence(
    phase: ServiceProvisioningPhase,
    code: ServiceProvisioningCode,
) -> FinalVerificationExecutionEvidence {
    FinalVerificationExecutionEvidence {
        manifest_version: 0,
        pre_environment_identity: None,
        post_environment_identity: None,
        fingerprint_f0: None,
        fingerprint_f1: None,
        commands: vec![],
        eligibility_reason: Some(FinalVerificationIneligibilityReason::ServiceProvisioning {
            phase,
            code,
        }),
    }
}

struct ServiceProbe {
    material: FinalVerificationResolvedMaterial,
    evidence: Mutex<Option<FinalVerificationExecutionEvidence>>,
    events: Arc<Mutex<Vec<&'static str>>>,
    lease_acquisitions: AtomicUsize,
    executions: AtomicUsize,
}

impl ServiceProbe {
    fn new(material: FinalVerificationResolvedMaterial) -> Self {
        Self {
            material,
            evidence: Mutex::new(None),
            events: Arc::new(Mutex::new(Vec::new())),
            lease_acquisitions: AtomicUsize::new(0),
            executions: AtomicUsize::new(0),
        }
    }
    fn with_evidence(mut self, evidence: FinalVerificationExecutionEvidence) -> Self {
        self.evidence = Mutex::new(Some(evidence));
        self
    }
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

struct ProbeLease {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl FinalVerificationInvocationLease for ProbeLease {
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async {
            self.events.lock().unwrap().push("lease-release");
            Ok(())
        })
    }
}

impl SlotHostCallbacks for ServiceProbe {
    fn final_verification_outcome_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationRecordingOutcome> {
        None
    }
    fn final_verification_evidence_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationExecutionEvidence> {
        self.events.lock().unwrap().push("evidence");
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.evidence.lock().unwrap().clone()
    }
    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        _verify_run_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<FinalVerificationResolvedMaterial>, String>>
                + Send
                + 'a,
        >,
    > {
        self.events.lock().unwrap().push("resolve");
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
            dyn Future<Output = Result<Box<dyn FinalVerificationInvocationLease>, String>>
                + Send
                + 'a,
        >,
    > {
        self.events.lock().unwrap().push("lease-acquire");
        self.lease_acquisitions.fetch_add(1, Ordering::SeqCst);
        let events = Arc::clone(&self.events);
        Box::pin(async move {
            Ok(Box::new(ProbeLease { events }) as Box<dyn FinalVerificationInvocationLease>)
        })
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
        panic!("build_mcp_state not needed in service provisioning tests")
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

struct Rig {
    ctx: SlotContext,
    request: FinalVerificationCoordinatorRequest,
    probe: Arc<ServiceProbe>,
}

async fn build_rig(probe: ServiceProbe) -> Rig {
    let db = crate::test_helpers::create_test_db();
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let run = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    let probe = Arc::new(probe);
    let ctx = crate::test_helpers::agent_context_from_db_with_callbacks(db, probe.clone());
    let request = FinalVerificationCoordinatorRequest {
        task_id: task.id,
        task_run_id: run,
        cancellation: CancellationToken::new(),
    };
    Rig {
        ctx,
        request,
        probe,
    }
}

async fn recorded_rows(ctx: &SlotContext, task_run_id: &str) -> usize {
    VerifyRunRepository::new(ctx.db.clone())
        .list_for_task_run(task_run_id)
        .await
        .unwrap()
        .len()
}

// ---------------------------------------------------------------------------
// service_type classification (bounded label)
// ---------------------------------------------------------------------------

#[test]
fn service_type_classification_is_bounded() {
    use djinn_telemetry::final_verification as tel;
    let (postgres, _) = tracking_material("preset-postgres-18", &["a"]);
    assert_eq!(
        classify_service_type(&postgres),
        tel::PROVISION_SERVICE_POSTGRES
    );
    let (redis, _) = tracking_material("preset-redis-7", &["a"]);
    assert_eq!(classify_service_type(&redis), tel::PROVISION_SERVICE_REDIS);
    let (rabbit, _) = tracking_material("preset-rabbitmq-4", &["a"]);
    assert_eq!(
        classify_service_type(&rabbit),
        tel::PROVISION_SERVICE_RABBITMQ
    );
    let (other, _) = tracking_material("preset-clickhouse", &["a"]);
    assert_eq!(classify_service_type(&other), tel::PROVISION_SERVICE_OTHER);
    // Empty plan classifies as `none`; distinct types collapse to `multiple`.
    assert_eq!(
        classify_service_type(&bare_material(&["a"])),
        tel::PROVISION_SERVICE_NONE
    );
    let mut multiple = bare_material(&["a"]);
    multiple.execution_request.service_provisioners = vec![
        Arc::new(TrackingProvisioner {
            preset: "preset-postgres-18".into(),
            counts: Arc::new(ProvisionCounts::default()),
        }),
        Arc::new(TrackingProvisioner {
            preset: "preset-redis-7".into(),
            counts: Arc::new(ProvisionCounts::default()),
        }),
    ];
    assert_eq!(
        classify_service_type(&multiple),
        tel::PROVISION_SERVICE_MULTIPLE
    );
}

// ---------------------------------------------------------------------------
// AC1 / AC3: provisioning failure is infrastructure-ineligible, records no row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn service_provisioning_failure_is_infrastructure_ineligible_and_records_no_row() {
    let cases = [
        (
            ServiceProvisioningPhase::Resolve,
            ServiceProvisioningCode::Unavailable,
        ),
        (
            ServiceProvisioningPhase::Proxy,
            ServiceProvisioningCode::Unavailable,
        ),
        (
            ServiceProvisioningPhase::Create,
            ServiceProvisioningCode::Rejected,
        ),
        (
            ServiceProvisioningPhase::Readiness,
            ServiceProvisioningCode::Timeout,
        ),
        (
            ServiceProvisioningPhase::Teardown,
            ServiceProvisioningCode::Rejected,
        ),
    ];
    for (phase, code) in cases {
        let (material, _counts) = tracking_material("preset-postgres-18", &["lint", "test"]);
        let probe =
            ServiceProbe::new(material).with_evidence(provisioning_failure_evidence(phase, code));
        let rig = build_rig(probe).await;
        let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
        let FinalVerificationRecordingOutcome::InfrastructureIneligible {
            phase: got_phase,
            code: got_code,
            ..
        } = outcome
        else {
            panic!("phase {phase:?} must be infrastructure-ineligible, got {outcome:?}");
        };
        assert_eq!(
            (got_phase, got_code),
            (phase, code),
            "bounded phase/code carried through"
        );
        // The lease is acquired then released; persistence is never reached.
        assert_eq!(
            rig.probe.events(),
            vec!["resolve", "lease-acquire", "evidence", "lease-release"],
            "phase {phase:?}"
        );
        assert_eq!(
            recorded_rows(&rig.ctx, &rig.request.task_run_id).await,
            0,
            "phase {phase:?} must record no passing verify-run row"
        );
    }
}

// ---------------------------------------------------------------------------
// AC5 continuity: a serviced plan still records exactly one pass on success
// and emits a bounded `complete`/`ok` provisioning outcome.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn serviced_forced_execution_records_one_pass() {
    let (material, _counts) = tracking_material("preset-postgres-18", &["lint", "test"]);
    let probe = ServiceProbe::new(material).with_evidence(passing_evidence("serviced-pass"));
    let rig = build_rig(probe).await;

    let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
    assert!(
        matches!(outcome, FinalVerificationRecordingOutcome::Stored { .. }),
        "serviced success must store one pass, got {outcome:?}"
    );
    assert_eq!(
        rig.probe.events(),
        vec!["resolve", "lease-acquire", "evidence", "lease-release"],
    );
    assert_eq!(recorded_rows(&rig.ctx, &rig.request.task_run_id).await, 1);
}

// ---------------------------------------------------------------------------
// AC4: provisioning telemetry is bounded; identifiers stay in the audit event
// ---------------------------------------------------------------------------

/// Count the emitted provisioning samples (value `1`), independent of label
/// ordering in the rendered exposition.
fn provisioning_sample_lines(rendered: &str) -> Vec<&str> {
    rendered
        .lines()
        .filter(|line| {
            line.starts_with("verify_service_provisioning_total{") && line.ends_with(" 1")
        })
        .collect()
}

/// True when exactly one emitted provisioning sample carries all three bounded
/// labels, regardless of the order the exporter renders them in.
fn has_provisioning_sample(rendered: &str, phase: &str, outcome: &str, service_type: &str) -> bool {
    provisioning_sample_lines(rendered).iter().any(|line| {
        line.contains(&format!("phase=\"{phase}\""))
            && line.contains(&format!("outcome=\"{outcome}\""))
            && line.contains(&format!("service_type=\"{service_type}\""))
    })
}

#[test]
fn provisioning_metric_labels_are_bounded_and_identifiers_never_leak() {
    let request = FinalVerificationCoordinatorRequest {
        task_id: "task-id-must-not-be-a-label".into(),
        task_run_id: "run-id-must-not-be-a-label".into(),
        cancellation: CancellationToken::new(),
    };
    let (material, _) = tracking_material("preset-postgres-18", &["lint", "test"]);
    let failure = provisioning_failure_evidence(
        ServiceProvisioningPhase::Create,
        ServiceProvisioningCode::Rejected,
    );
    let success = passing_evidence("fingerprint-must-not-be-a-label");

    let ((), rendered) = djinn_telemetry::render_isolated(|| {
        emit_service_provisioning_outcome(
            &request,
            "attempt-id-must-not-be-a-label",
            &material,
            &failure,
        );
        emit_service_provisioning_outcome(
            &request,
            "attempt-id-must-not-be-a-label",
            &material,
            &success,
        );
    });
    assert_eq!(
        provisioning_sample_lines(&rendered).len(),
        2,
        "exactly two bounded provisioning samples were emitted: {rendered}"
    );
    assert!(
        has_provisioning_sample(&rendered, "create", "rejected", "postgres"),
        "a create/rejected/postgres sample must be present: {rendered}"
    );
    assert!(
        has_provisioning_sample(&rendered, "complete", "ok", "postgres"),
        "a complete/ok/postgres sample must be present: {rendered}"
    );
    for identifier in [
        "task-id-must-not-be-a-label",
        "run-id-must-not-be-a-label",
        "attempt-id-must-not-be-a-label",
        "fingerprint-must-not-be-a-label",
        "identity-stable",
    ] {
        assert!(
            !rendered.contains(identifier),
            "identifier {identifier} must never appear in a metric label"
        );
    }

    // The structured audit event does carry the identifiers.
    let events = capture_events(|| {
        emit_service_provisioning_outcome(
            &request,
            "attempt-id-must-not-be-a-label",
            &material,
            &success,
        )
    });
    for identifier in [
        "task-id-must-not-be-a-label",
        "run-id-must-not-be-a-label",
        "attempt-id-must-not-be-a-label",
        "fingerprint-must-not-be-a-label",
        "identity-stable",
    ] {
        assert!(
            events.contains(identifier),
            "audit event must carry {identifier}: {events}"
        );
    }
}

// A minimal tracing capture, mirroring the telemetry-contract test helper.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);
struct CapturedLogsWriter(Arc<Mutex<Vec<u8>>>);
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogsWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CapturedLogsWriter(Arc::clone(&self.0))
    }
}
impl Write for CapturedLogsWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn capture_events(f: impl FnOnce()) -> String {
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_target(false)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let guard = tracing::dispatcher::set_default(&dispatch);
    f();
    drop(guard);
    String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
}

// ---------------------------------------------------------------------------
// AC2: a matching reuse returns the stored pass without provisioning or
// executing anything (Postgres-backed).
// ---------------------------------------------------------------------------

async fn init_verifiable_repo(prefix: &str) -> tempfile::TempDir {
    let tree = crate::test_helpers::test_tempdir(prefix);
    run_git_command_in(tree.path(), vec!["init".into()])
        .await
        .expect("git init");
    for args in [
        vec!["config", "--local", "user.email", "test@test.com"],
        vec!["config", "--local", "user.name", "Test User"],
        vec!["config", "--local", "commit.gpgsign", "false"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .expect("git config");
    }
    std::fs::write(tree.path().join("authored.txt"), "state").unwrap();
    for args in [
        vec!["add", "authored.txt"],
        vec!["commit", "-m", "init"],
        vec!["branch", "-m", "main"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .expect("git populate");
    }
    tree
}

#[tokio::test]
async fn matching_reuse_returns_stored_pass_without_provisioning_or_execution() {
    let tree = init_verifiable_repo("reuse-services-").await;
    let mut material = reuse_material(tree.path().into());
    let counts = Arc::new(ProvisionCounts::default());
    material.execution_request.service_provisioners = vec![Arc::new(TrackingProvisioner {
        preset: "preset-postgres-18".into(),
        counts: Arc::clone(&counts),
    })];

    let fingerprint =
        match compute_verification_input_fingerprint(&material.execution_request.worktree)
            .await
            .unwrap()
        {
            VerificationInputFingerprint::Available(digest) => digest.fingerprint,
            VerificationInputFingerprint::Unavailable(reason) => panic!("fingerprint: {reason}"),
        };
    let identity = EnvironmentIdentityV1::derive(
        (material.execution_request.resolve_environment_identity)().unwrap(),
    )
    .unwrap();

    let probe = ServiceProbe::new(material.clone());
    let rig = build_rig(probe).await;
    let task = rig.ctx.load_task(&rig.request.task_id).await.unwrap();
    SettingsRepository::new(rig.ctx.db.clone(), rig.ctx.event_bus.clone())
        .set(
            &format!("project.{}.verify_run_reuse_enabled", task.project_id),
            "true",
        )
        .await
        .unwrap();

    let required_commands = [
        RequiredFinalVerificationCommand {
            descriptor_id: "format",
        },
        RequiredFinalVerificationCommand {
            descriptor_id: "slot-clippy",
        },
    ];
    let ordered_commands = serde_json::json!([
        {"descriptor_id":"format","result":"pass","passed":true},
        {"descriptor_id":"slot-clippy","result":"pass","passed":true}
    ]);
    let covered_checks = serde_json::json!(["format", "slot-clippy"]);
    let identity_json = serde_json::from_str(&identity.canonical_json).unwrap();
    VerifyRunRepository::new(rig.ctx.db.clone())
        .record_eligible_final_verification_pass(RecordEligibleFinalVerificationPassParams {
            id: "reuse-services-row",
            task_run_id: &rig.request.task_run_id,
            verify_source: "worker",
            verify_run_id: "seeded-run",
            verification_attempt_id: "seeded-attempt",
            required_commands: &required_commands,
            ordered_commands: &ordered_commands,
            covered_checks: &covered_checks,
            required_checks: &material.required_checks,
            verification_input_fingerprint: &fingerprint,
            manifest_version: "manifest-v1",
            environment_identity_json: &identity_json,
            environment_identity_digest: &identity.digest,
            environment_identity_version: "identity-v1",
            completed_at: "2099-02-02T03:04:05Z",
            diff_fingerprint: &material.diff_fingerprint,
        })
        .await
        .unwrap();

    let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
    let FinalVerificationRecordingOutcome::Reused { evidence, .. } = outcome else {
        panic!("matching reuse must return the stored pass, got {outcome:?}");
    };
    assert_eq!(evidence.persisted_run_id, "reuse-services-row");
    // No provisioning, no lease, no execution occurred on the reuse hit.
    assert_eq!(counts.create.load(Ordering::SeqCst), 0);
    assert_eq!(counts.ready.load(Ordering::SeqCst), 0);
    assert_eq!(counts.delete.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.lease_acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.executions.load(Ordering::SeqCst), 0);
    assert_eq!(rig.probe.events(), vec!["resolve", "resolve"]);
    assert_eq!(recorded_rows(&rig.ctx, &rig.request.task_run_id).await, 1);
}
