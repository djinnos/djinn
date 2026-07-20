// djinn:allow-oversize — integration tests exercise the complete recording and
// completion boundaries with shared pod-shaped fixtures.
//! End-to-end regression matrix for the authoritative post-authoring writer.
//!
//! Every case drives the real `coordinate_final_verification` boundary —
//! canonical resolution, invocation-lease acquisition/release, injected
//! executor evidence validation, and the durable writer — with only the
//! hermetic execution replaced by deterministic injected evidence
//! (`final_verification_evidence_for_test`). The typed-outcome seam
//! (`final_verification_outcome_for_test`) is disabled so no case can bypass
//! resolution, leasing, validation, or persistence.
//!
//! The success case proves a worker completion records exactly one complete
//! pass; every failure case proves the bounded recording outcome
//! (`Ineligible`/`Error`) and the absence of a passing row. Recording is
//! unconditional: nothing here enables or consults `verify_run_reuse_enabled`,
//! preserving the writer-first/shadow rollout.

use super::*;
use crate::host::{ResolvedMcpTools, SlotHostCallbacks};
use djinn_core::models::Task;
use djinn_core::{
    canonical_verify::{CanonicalCommandDescriptorV1, EnvironmentIdentityV1},
    models::VerifySource,
};
use djinn_db::repositories::{
    task_run::{CreateTaskRunParams, TaskRunRepository},
    verify_run::VerifyRunRepository,
};
use djinn_git::{
    VerificationInputDigestV1, VerificationInputFingerprint, VerificationInputFingerprintConfig,
    compute_verification_input_fingerprint, run_git_command_in,
};
use djinn_sandbox::final_verification_execution::{
    FinalVerificationCommandEvidence, FinalVerificationExecutionEvidence,
    FinalVerificationIneligibilityReason,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Deterministic evidence/material builders
// ---------------------------------------------------------------------------

fn command(id: &str, at: u128) -> FinalVerificationCommandEvidence {
    FinalVerificationCommandEvidence {
        descriptor: CanonicalCommandDescriptorV1 {
            check_id: id.into(),
            executable: "hermetic-tool".into(),
            argv: vec![id.into()],
            working_directory: ".".into(),
            environment_names: vec![],
            timeout_seconds: 60,
            descriptor_revision: 1,
        },
        started_at_unix_millis: at,
        completed_at_unix_millis: at + 1,
        exit_code: Some(0),
        timed_out: false,
    }
}
fn fingerprint(value: &str) -> VerificationInputDigestV1 {
    VerificationInputDigestV1 {
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
fn passing(value: &str) -> FinalVerificationExecutionEvidence {
    FinalVerificationExecutionEvidence {
        manifest_version: 1,
        pre_environment_identity: Some(identity("stable")),
        post_environment_identity: Some(identity("stable")),
        fingerprint_f0: Some(fingerprint(value)),
        fingerprint_f1: Some(fingerprint(value)),
        commands: vec![command("lint", 10), command("test", 20)],
        eligibility_reason: None,
    }
}
fn material(checks: Vec<String>) -> FinalVerificationResolvedMaterial {
    FinalVerificationResolvedMaterial {
        execution_request: FinalVerificationExecutionRequest {
            worktree: PathBuf::new(),
            resolve_environment_identity: Arc::new(|| panic!("injected evidence only")),
            fingerprint_config: VerificationInputFingerprintConfig::default(),
            tool_runtime: vec![],
            read_only_external_mounts: vec![],
            output_directories: vec![],
        },
        verify_source: VerifySource::Worker,
        required_checks: checks,
        diff_fingerprint: "audit-diff".into(),
    }
}

// ---------------------------------------------------------------------------
// Probe host callbacks: force the full coordinator path with injected seams
// ---------------------------------------------------------------------------

/// Records every coordinator boundary crossing in order and injects
/// deterministic resolution material and executor evidence. Returning `None`
/// from `final_verification_outcome_for_test` disables the typed-outcome
/// shortcut so each test traverses the real
/// resolve → lease → evidence → validate → persist pipeline.
struct CoordinatorProbe {
    events: Arc<Mutex<Vec<&'static str>>>,
    material: FinalVerificationResolvedMaterial,
    evidence: Mutex<Option<FinalVerificationExecutionEvidence>>,
    resolve_error: Option<String>,
    /// Resolve the typed "no plan configured" signal (`Ok(None)`).
    resolve_not_configured: bool,
    cancel_on_evidence: bool,
    resolved_ids: Mutex<Vec<(String, String)>>,
    /// Tracks every bounded lookup outcome emitted during a coordinator call.
    consultation_outcomes: Mutex<Vec<(&'static str, &'static str)>>,
    lease_requests: Mutex<usize>,
    lease_acquisitions: Mutex<usize>,
    canonical_executions: Mutex<usize>,
}

impl CoordinatorProbe {
    fn new(material: FinalVerificationResolvedMaterial) -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            material,
            evidence: Mutex::new(None),
            resolve_error: None,
            resolve_not_configured: false,
            cancel_on_evidence: false,
            resolved_ids: Mutex::new(Vec::new()),
            consultation_outcomes: Mutex::new(Vec::new()),
            lease_requests: Mutex::new(0),
            lease_acquisitions: Mutex::new(0),
            canonical_executions: Mutex::new(0),
        }
    }
    fn inject_evidence(&self, evidence: FinalVerificationExecutionEvidence) {
        *self.evidence.lock().unwrap() = Some(evidence);
    }
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
    fn resolved_ids(&self) -> Vec<(String, String)> {
        self.resolved_ids.lock().unwrap().clone()
    }
    fn consultation_outcomes(&self) -> Vec<(&'static str, &'static str)> {
        self.consultation_outcomes.lock().unwrap().clone()
    }
}

/// The normal invocation lease. Release is recorded on the shared event log so
/// tests can prove the lease is released on every outcome path.
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

impl SlotHostCallbacks for CoordinatorProbe {
    fn record_final_verification_consultation_outcome_for_test(
        &self,
        outcome: &'static str,
        reason: &'static str,
    ) {
        self.consultation_outcomes
            .lock()
            .unwrap()
            .push((outcome, reason));
    }
    fn final_verification_outcome_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationRecordingOutcome> {
        // Never short-circuit: these tests prove the full coordinator path.
        None
    }
    fn final_verification_evidence_for_test(
        &self,
        request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationExecutionEvidence> {
        self.events.lock().unwrap().push("evidence");
        *self.canonical_executions.lock().unwrap() += 1;
        if self.cancel_on_evidence {
            request.cancellation.cancel();
        }
        self.evidence.lock().unwrap().clone()
    }
    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        verification_attempt_id: &'a str,
        verify_run_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<FinalVerificationResolvedMaterial>, String>>
                + Send
                + 'a,
        >,
    > {
        self.events.lock().unwrap().push("resolve");
        self.resolved_ids
            .lock()
            .unwrap()
            .push((verification_attempt_id.to_owned(), verify_run_id.to_owned()));
        let material = self.material.clone();
        let error = self.resolve_error.clone();
        let not_configured = self.resolve_not_configured;
        Box::pin(async move {
            match error {
                Some(detail) => Err(detail),
                None if not_configured => Ok(None),
                None => Ok(Some(material)),
            }
        })
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
        *self.lease_requests.lock().unwrap() += 1;
        *self.lease_acquisitions.lock().unwrap() += 1;
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
        panic!("build_mcp_state not needed in recording tests")
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
// Fixtures
// ---------------------------------------------------------------------------

struct Rig {
    ctx: SlotContext,
    request: FinalVerificationCoordinatorRequest,
    probe: Arc<CoordinatorProbe>,
}

/// Build a task/task-run fixture whose host callbacks drive the real
/// coordinator boundary with the injected material/evidence from `configure`.
async fn build_rig(required_checks: &[&str], configure: impl FnOnce(&mut CoordinatorProbe)) -> Rig {
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
    let mut probe = CoordinatorProbe::new(material(
        required_checks
            .iter()
            .map(|check| check.to_string())
            .collect(),
    ));
    configure(&mut probe);
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

/// A rig whose injected evidence is a canonical two-command pass.
async fn passing_rig(fingerprint_value: &str) -> Rig {
    build_rig(&["lint", "test"], |probe| {
        probe.inject_evidence(passing(fingerprint_value));
    })
    .await
}

async fn recorded_rows(
    ctx: &SlotContext,
    task_run_id: &str,
) -> Vec<djinn_core::models::VerifyRunRecord> {
    VerifyRunRepository::new(ctx.db.clone())
        .list_for_task_run(task_run_id)
        .await
        .unwrap()
}

/// The default-test-callback fixture (typed outcome seam returns `Stored`) used
/// to prove cancellation precedes even the test-only outcome shortcut.
async fn fixture() -> (SlotContext, FinalVerificationCoordinatorRequest) {
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
    let ctx = crate::test_helpers::agent_context_from_db(db, CancellationToken::new());
    let request = FinalVerificationCoordinatorRequest {
        task_id: task.id,
        task_run_id: run,
        cancellation: CancellationToken::new(),
    };
    (ctx, request)
}

// ---------------------------------------------------------------------------
// Criterion 1: canonical multi-command success records exactly one complete row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn canonical_multi_command_completion_records_one_complete_row() {
    let rig = passing_rig("post-authoring-fingerprint").await;
    let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
    let FinalVerificationRecordingOutcome::Stored {
        verification_attempt_id,
        verify_run_id,
        evidence,
    } = outcome
    else {
        panic!("canonical completion must store one pass, got {outcome:?}");
    };
    // The completion traversed resolution, the invocation lease, the injected
    // executor evidence, and lease release — in that order — before the
    // coordinator's own persistence call wrote the row asserted below.
    assert_eq!(
        rig.probe.events(),
        vec!["resolve", "lease-acquire", "evidence", "lease-release"]
    );
    // Resolution observed the same single attempt/run IDs that the writer
    // persisted; no second attempt or run ID was ever opened.
    assert_eq!(
        rig.probe.resolved_ids(),
        vec![(verification_attempt_id.clone(), verify_run_id.clone())]
    );
    let rows = recorded_rows(&rig.ctx, &rig.request.task_run_id).await;
    assert_eq!(rows.len(), 1, "exactly one passing row must be recorded");
    let row = &rows[0];
    assert_eq!(row.result, "pass");
    assert_eq!(
        row.verification_attempt_id.as_deref(),
        Some(verification_attempt_id.as_str())
    );
    assert_eq!(row.verify_run_id, verify_run_id);
    assert_eq!(evidence.persisted_run_id, row.id);
    assert_eq!(evidence.completed_at, row.completed_at);
    assert_eq!(
        evidence.ordered_commands,
        row.ordered_commands.clone().unwrap()
    );
    assert_eq!(evidence.covered_checks, row.covered_checks.clone().unwrap());
    assert_eq!(evidence.required_checks, vec!["lint", "test"]);
    assert_eq!(
        evidence.verification_input_fingerprint,
        "post-authoring-fingerprint"
    );
    assert_eq!(evidence.manifest_version, "manifest-v1");
    assert_eq!(evidence.environment_identity_digest, "identity-stable");
    assert_eq!(row.source_phase.as_deref(), Some("final_verification"));
    // Ordered passing commands in plan order.
    let ordered: Vec<(&str, &str, bool)> = row
        .ordered_commands
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["descriptor_id"].as_str().unwrap(),
                entry["result"].as_str().unwrap(),
                entry["passed"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        ordered,
        vec![("lint", "pass", true), ("test", "pass", true)]
    );
    // Required checks are covered.
    assert_eq!(
        row.covered_checks,
        Some(serde_json::json!(["lint", "test"]))
    );
    // Post-authoring fingerprint, manifest version, and unchanged environment
    // identity are recorded on the row.
    assert_eq!(
        row.verification_input_fingerprint.as_deref(),
        Some("post-authoring-fingerprint")
    );
    assert_eq!(row.manifest_version.as_deref(), Some("manifest-v1"));
    assert_eq!(
        row.environment_identity_digest.as_deref(),
        Some("identity-stable")
    );
    assert_eq!(
        row.environment_identity_version.as_deref(),
        Some("identity-v1")
    );
    assert_eq!(
        row.environment_identity_json.as_ref().unwrap()["environment"],
        "stable"
    );
}

// ---------------------------------------------------------------------------
// Criterion 2: every failure shape records no passing row and a bounded outcome
// ---------------------------------------------------------------------------

/// Executor-reported ineligibility: partial command failure, timeout,
/// unsupported/non-hermetic or ad-hoc execution, sandbox violation, and the
/// executor-observed fingerprint/environment/input/identity/manifest failures.
/// Each case traverses resolve → lease → evidence → release and writes nothing.
#[tokio::test]
async fn ineligible_executor_evidence_records_no_row() {
    use FinalVerificationIneligibilityReason as R;
    let cases: Vec<(R, &str)> = vec![
        (
            R::CommandFailed {
                check_id: "test".into(),
                exit_code: Some(1),
            },
            "CommandFailed",
        ),
        (
            R::CommandTimedOut {
                check_id: "test".into(),
            },
            "CommandTimedOut",
        ),
        (R::NonHermeticPlan, "NonHermeticPlan"),
        (
            R::UndeclaredCommandEnvironment {
                name: "AD_HOC".into(),
            },
            "UndeclaredCommandEnvironment",
        ),
        (
            R::SandboxViolation {
                detail: "escaped sandbox".into(),
            },
            "SandboxViolation",
        ),
        (R::FingerprintChanged, "FingerprintChanged"),
        (R::EnvironmentChanged, "EnvironmentChanged"),
        (
            R::FingerprintUnavailable {
                detail: "input".into(),
            },
            "FingerprintUnavailable",
        ),
        (
            R::EnvironmentIdentityUnavailable {
                detail: "identity".into(),
            },
            "EnvironmentIdentityUnavailable",
        ),
        (
            R::ManifestBindingMismatch {
                detail: "manifest".into(),
            },
            "ManifestBindingMismatch",
        ),
    ];
    for (reason, needle) in cases {
        let evidence = FinalVerificationExecutionEvidence {
            eligibility_reason: Some(reason),
            ..passing("never-persisted")
        };
        let rig = build_rig(&["lint", "test"], |probe| {
            probe.inject_evidence(evidence);
        })
        .await;
        let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
        let FinalVerificationRecordingOutcome::Ineligible {
            verification_attempt_id,
            reason: detail,
        } = outcome
        else {
            panic!("case {needle} must be ineligible, got {outcome:?}");
        };
        assert!(
            detail.contains(needle),
            "case {needle} must surface its bounded reason, got {detail}"
        );
        // The case still resolved and leased exactly once, and the lease was
        // released on the ineligible path.
        assert_eq!(
            rig.probe.events(),
            vec!["resolve", "lease-acquire", "evidence", "lease-release"],
            "case {needle}"
        );
        assert_eq!(
            rig.probe.resolved_ids()[0].0,
            verification_attempt_id,
            "case {needle}"
        );
        assert!(
            recorded_rows(&rig.ctx, &rig.request.task_run_id)
                .await
                .is_empty(),
            "case {needle} must record no passing row"
        );
    }
}

/// Writer-side consistency boundaries: F0/F1 mutation and environment change
/// observed between execution and persistence, a required-check gap the
/// durable writer fails closed on, and a repository insert failure.
#[tokio::test]
async fn consistency_and_writer_boundaries_record_no_row() {
    // F0/F1 mutation: the tree changed between the pre- and post-execution
    // fingerprints, detected at the persistence consistency boundary.
    let mut f1_mutation = passing("f0");
    f1_mutation.fingerprint_f1 = Some(fingerprint("f1-mutated"));
    // Environment change: post-execution identity differs from pre-execution.
    let mut environment_change = passing("same");
    environment_change.post_environment_identity = Some(identity("changed"));
    for (label, evidence) in [
        ("f0/f1 mutation", f1_mutation),
        ("environment change", environment_change),
    ] {
        let rig = build_rig(&["lint", "test"], |probe| {
            probe.inject_evidence(evidence);
        })
        .await;
        let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
        let FinalVerificationRecordingOutcome::Ineligible { reason, .. } = outcome else {
            panic!("{label} must be ineligible, got {outcome:?}");
        };
        assert_eq!(
            reason, "verification consistency boundary changed",
            "{label}"
        );
        assert_eq!(
            rig.probe.events(),
            vec!["resolve", "lease-acquire", "evidence", "lease-release"],
            "{label}"
        );
        assert!(
            recorded_rows(&rig.ctx, &rig.request.task_run_id)
                .await
                .is_empty(),
            "{label} must record no passing row"
        );
    }

    // Required-check gap: the evidence covers lint+test but the resolved plan
    // also requires docs; the durable writer fails closed instead of storing
    // an incomplete pass.
    let rig = build_rig(&["lint", "test", "docs"], |probe| {
        probe.inject_evidence(passing("gap"));
    })
    .await;
    let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
    let FinalVerificationRecordingOutcome::Error { detail, .. } = outcome else {
        panic!("required-check gap must error, got {outcome:?}");
    };
    assert!(
        detail.contains("final verification insert failed"),
        "required-check gap must surface the bounded writer error, got {detail}"
    );
    assert!(
        recorded_rows(&rig.ctx, &rig.request.task_run_id)
            .await
            .is_empty(),
        "required-check gap must record no passing row"
    );

    // Insert failure: the repository rejects the write (unknown task run), and
    // the coordinator surfaces the bounded error without a row.
    let rig = passing_rig("insert").await;
    let bad = FinalVerificationCoordinatorRequest {
        task_run_id: "missing-run".into(),
        ..rig.request.clone()
    };
    let outcome = coordinate_final_verification(bad, &rig.ctx).await;
    let FinalVerificationRecordingOutcome::Error { detail, .. } = outcome else {
        panic!("insert failure must error, got {outcome:?}");
    };
    assert!(
        detail.contains("final verification insert failed"),
        "insert failure must surface the bounded writer error, got {detail}"
    );
    assert!(
        recorded_rows(&rig.ctx, &rig.request.task_run_id)
            .await
            .is_empty(),
        "insert failure must record no passing row"
    );
}

/// Input, identity, and manifest resolution failures injected at the
/// resolution boundary: the coordinator errors before any lease is acquired
/// and records nothing.
#[tokio::test]
async fn resolution_failures_record_no_row_and_never_lease() {
    for (label, detail) in [
        ("input resolution", "canonical plan input resolution failed"),
        (
            "identity resolution",
            "environment identity resolution failed",
        ),
        ("manifest resolution", "manifest resolution failed"),
    ] {
        let rig = build_rig(&["lint", "test"], |probe| {
            probe.resolve_error = Some(detail.to_owned());
            probe.inject_evidence(passing("never-persisted"));
        })
        .await;
        let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
        let FinalVerificationRecordingOutcome::Error { detail: got, .. } = outcome else {
            panic!("{label} failure must error, got {outcome:?}");
        };
        assert_eq!(got, detail, "{label} must surface the resolution error");
        assert_eq!(
            rig.probe.events(),
            vec!["resolve"],
            "{label} failure must never acquire the invocation lease"
        );
        assert!(
            recorded_rows(&rig.ctx, &rig.request.task_run_id)
                .await
                .is_empty(),
            "{label} failure must record no passing row"
        );
    }
}

/// Defect regression (post-#2270 production outage): a project with no
/// final-verification plan is a typed `NotConfigured` skip, not an error. The
/// coordinator returns before any lease/execution/persistence, and the
/// completion-intent path proceeds with `Ok(None)` — no fabricated evidence,
/// no blocked submission.
#[tokio::test]
async fn unconfigured_plan_is_typed_skip_and_submission_proceeds_without_evidence() {
    let rig = build_rig(&["lint", "test"], |probe| {
        probe.resolve_not_configured = true;
        // Evidence is injected but must never be consulted: the typed skip
        // returns before the executor seam.
        probe.inject_evidence(passing("never-executed"));
    })
    .await;
    let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
    assert!(
        matches!(
            outcome,
            FinalVerificationRecordingOutcome::NotConfigured { .. }
        ),
        "unconfigured plan must produce the typed skip, got {outcome:?}"
    );
    assert_eq!(
        rig.probe.events(),
        vec!["resolve"],
        "the typed skip must never lease, execute, or release"
    );
    assert!(
        recorded_rows(&rig.ctx, &rig.request.task_run_id)
            .await
            .is_empty(),
        "the typed skip must record no verify-run row"
    );

    // The completion-intent boundary maps the typed skip to "proceed without
    // evidence": Ok(None), and the intent carries no evidence.
    let mut intent = crate::output_parser::CompletionIntent {
        finalize_payload: serde_json::json!({}),
        tool_use_id: "finalize-tool-use".to_owned(),
        final_verification_evidence: None,
        final_verification_disposition: crate::output_parser::FinalVerificationDisposition::Pending,
    };
    let proceed = verify_completion_intent(
        &mut intent,
        &rig.request.task_id,
        Some(&rig.request.task_run_id),
        CancellationToken::new(),
        &rig.ctx,
        "submit_work",
    )
    .await;
    assert!(
        matches!(proceed, Ok(None)),
        "unconfigured plan must let the submission proceed without evidence, got {proceed:?}"
    );
    assert!(
        intent.final_verification_evidence.is_none(),
        "no evidence may be synthesized for an unconfigured plan"
    );
    assert_eq!(
        rig.probe.events(),
        vec!["resolve", "resolve"],
        "the completion-intent path must also stop at resolution"
    );
    assert!(
        recorded_rows(&rig.ctx, &rig.request.task_run_id)
            .await
            .is_empty(),
        "the completion-intent path must record no verify-run row"
    );
}

/// Cancellation observed while injected executor evidence is in flight:
/// ineligible, lease released, nothing recorded.
#[tokio::test]
async fn cancellation_during_execution_records_no_row() {
    let rig = build_rig(&["lint", "test"], |probe| {
        probe.cancel_on_evidence = true;
        probe.inject_evidence(passing("cancelled"));
    })
    .await;
    let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
    let FinalVerificationRecordingOutcome::Ineligible { reason, .. } = outcome else {
        panic!("cancellation must be ineligible, got {outcome:?}");
    };
    assert_eq!(reason, "cancelled during execution");
    assert_eq!(
        rig.probe.events(),
        vec!["resolve", "lease-acquire", "evidence", "lease-release"]
    );
    assert!(
        recorded_rows(&rig.ctx, &rig.request.task_run_id)
            .await
            .is_empty(),
        "cancellation must record no passing row"
    );
}

/// Cancellation is authoritative even when the test-only typed-outcome seam
/// would have produced a stored pass.
#[tokio::test]
async fn cancellation_precedes_injected_stored_outcome() {
    let (ctx, request) = fixture().await;
    request.cancellation.cancel();
    assert!(matches!(
        coordinate_final_verification(request.clone(), &ctx).await,
        FinalVerificationRecordingOutcome::Ineligible { reason, .. }
            if reason == "cancelled before resolution"
    ));
    assert!(
        recorded_rows(&ctx, &request.task_run_id).await.is_empty(),
        "cancellation must record no passing row"
    );
}

// ---------------------------------------------------------------------------
// Criterion 3: setup-time pre_verification records nothing; only the
// post-authoring fingerprint (reflecting the post-setup edit) is persisted
// ---------------------------------------------------------------------------

/// Initialize a real git worktree whose `main` ref and HEAD make the V1
/// verification-input fingerprint computable by the production path.
async fn init_verifiable_repo(prefix: &str) -> tempfile::TempDir {
    let tree = crate::test_helpers::test_tempdir(prefix);
    run_git_command_in(tree.path(), vec!["init".into()])
        .await
        .expect("initialize git repository");
    for args in [
        vec!["config", "--local", "user.email", "test@test.com"],
        vec!["config", "--local", "user.name", "Test User"],
        vec!["config", "--local", "commit.gpgsign", "false"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .expect("configure git repository");
    }
    std::fs::write(tree.path().join("authored.txt"), "setup state").unwrap();
    for args in [
        vec!["add", "authored.txt"],
        vec!["commit", "-m", "init"],
        vec!["branch", "-m", "main"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .expect("populate git repository");
    }
    tree
}

/// Compute the fingerprint through the same production path the final
/// verification executor uses (`compute_verification_input_fingerprint`).
async fn post_authoring_digest(tree: &Path) -> VerificationInputDigestV1 {
    match compute_verification_input_fingerprint(tree)
        .await
        .expect("fingerprint computation runs")
    {
        VerificationInputFingerprint::Available(digest) => digest,
        VerificationInputFingerprint::Unavailable(reason) => {
            panic!("fingerprint must be available: {reason}")
        }
    }
}

#[tokio::test]
async fn setup_pre_verification_records_nothing_and_stores_only_post_authoring_fingerprint() {
    // A real worktree in its setup-time state.
    let tree = init_verifiable_repo("setup-vs-final-").await;
    // The completion fixture; the injected executor evidence is supplied after
    // the post-setup edit so it must carry the real post-authoring digest.
    let rig = build_rig(&["lint", "test"], |_probe| {}).await;

    // Simulate setup-time `pre_verification` the way `resolve_setup_context`
    // runs lifecycle hooks: `sh -c` inside the worktree (see djinn-agent
    // `commands::run_commands`). The companion test in
    // `djinn-agent/src/actors/slot/lifecycle/setup.rs` invokes the real setup
    // entry point and proves the same exclusion there.
    let hook = std::process::Command::new("sh")
        .arg("-c")
        .arg("printf 'setup ran' > setup_marker.txt")
        .current_dir(tree.path())
        .output()
        .expect("run setup hook");
    assert!(hook.status.success(), "setup hook must run in the worktree");
    assert_eq!(
        std::fs::read_to_string(tree.path().join("setup_marker.txt")).unwrap(),
        "setup ran"
    );

    // Setup opened no final-verification attempt (the host boundary was never
    // consulted) and recorded no reusable pass.
    assert_eq!(
        rig.probe.events(),
        Vec::<&'static str>::new(),
        "setup must never resolve, lease, or execute final verification"
    );
    assert!(
        recorded_rows(&rig.ctx, &rig.request.task_run_id)
            .await
            .is_empty(),
        "setup must record no verify-run pass"
    );

    // The fingerprint a hypothetical setup-time pass would have reused.
    let setup_digest = post_authoring_digest(tree.path()).await;

    // The worker authors after setup: edit the tracked tree.
    std::fs::write(tree.path().join("authored.txt"), "post-authoring edit").unwrap();
    let post_digest = post_authoring_digest(tree.path()).await;
    assert_ne!(
        setup_digest.fingerprint, post_digest.fingerprint,
        "the post-authoring fingerprint must reflect the edit"
    );

    // Worker completion: the executor evidence carries the real post-authoring
    // digest at both consistency boundaries, exactly as the production
    // executor computes F0/F1 around a non-mutating run.
    rig.probe
        .inject_evidence(FinalVerificationExecutionEvidence {
            fingerprint_f0: Some(post_digest.clone()),
            fingerprint_f1: Some(post_digest.clone()),
            ..passing("unused")
        });
    let outcome = coordinate_final_verification(rig.request.clone(), &rig.ctx).await;
    assert!(
        matches!(outcome, FinalVerificationRecordingOutcome::Stored { .. }),
        "post-authoring completion must store one pass, got {outcome:?}"
    );
    let rows = recorded_rows(&rig.ctx, &rig.request.task_run_id).await;
    assert_eq!(rows.len(), 1, "exactly one passing row may exist");
    // The only persisted fingerprint is post-authoring and reflects the edit —
    // never the setup-time tree state.
    assert_eq!(
        rows[0].verification_input_fingerprint.as_deref(),
        Some(post_digest.fingerprint.as_str())
    );
    assert_ne!(
        rows[0].verification_input_fingerprint.as_deref(),
        Some(setup_digest.fingerprint.as_str())
    );
    assert_eq!(rows[0].result, "pass");
    assert_eq!(rows[0].source_phase.as_deref(), Some("final_verification"));
}

// ---------------------------------------------------------------------------
// Criterion 4: pod-shaped unconfigured-plan legacy decision before consultation
//
// These tests insert task_runs exactly as djinn-k8s dispatch does
// (workspace_path = NULL) and prove that every completion path — model-called
// submit_work, auto-submit, and controlled termination — resolves the plan's
// unconfigured state before any lookup, lease, or execution, emits exactly one
// `disabled`/`plan_unconfigured` audit, and finalizes through the legacy skip
// with no evidence, no canonical command, no invocation lease, and no verify-
// run row. A configured-plan control with no workspace stays fail-closed.
// ---------------------------------------------------------------------------

/// A probe variant that can resolve a configured plan but fail on a missing
/// worktree (mirroring the production host adapter's worktree requirement for
/// configured plans) while also tracking all consultation/lease/execution
/// counters needed by the pod-shaped regression matrix.
struct PodProbe {
    events: Arc<Mutex<Vec<&'static str>>>,
    resolve_not_configured: bool,
    /// When false and the plan is configured, resolve returns an error
    /// (simulating "task run has no worktree" for a pod with no persisted path).
    configured_with_worktree: bool,
    consultation_outcomes: Mutex<Vec<(&'static str, &'static str)>>,
    lease_requests: Mutex<usize>,
    lease_acquisitions: Mutex<usize>,
    canonical_executions: Mutex<usize>,
    resolved_ids: Mutex<Vec<(String, String)>>,
}

impl PodProbe {
    fn unconfigured() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            resolve_not_configured: true,
            configured_with_worktree: false,
            consultation_outcomes: Mutex::new(Vec::new()),
            lease_requests: Mutex::new(0),
            lease_acquisitions: Mutex::new(0),
            canonical_executions: Mutex::new(0),
            resolved_ids: Mutex::new(Vec::new()),
        }
    }
    fn configured_no_worktree() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            resolve_not_configured: false,
            configured_with_worktree: false,
            consultation_outcomes: Mutex::new(Vec::new()),
            lease_requests: Mutex::new(0),
            lease_acquisitions: Mutex::new(0),
            canonical_executions: Mutex::new(0),
            resolved_ids: Mutex::new(Vec::new()),
        }
    }
}

struct PodProbeLease;

impl FinalVerificationInvocationLease for PodProbeLease {
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

impl SlotHostCallbacks for PodProbe {
    fn record_final_verification_consultation_outcome_for_test(
        &self,
        outcome: &'static str,
        reason: &'static str,
    ) {
        self.consultation_outcomes
            .lock()
            .unwrap()
            .push((outcome, reason));
    }
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
        *self.canonical_executions.lock().unwrap() += 1;
        None
    }
    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        verification_attempt_id: &'a str,
        verify_run_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<FinalVerificationResolvedMaterial>, String>>
                + Send
                + 'a,
        >,
    > {
        self.events.lock().unwrap().push("resolve");
        self.resolved_ids
            .lock()
            .unwrap()
            .push((verification_attempt_id.to_owned(), verify_run_id.to_owned()));
        let not_configured = self.resolve_not_configured;
        let has_worktree = self.configured_with_worktree;
        let mat = material(vec!["lint".to_owned(), "test".to_owned()]);
        Box::pin(async move {
            if not_configured {
                Ok(None)
            } else if !has_worktree {
                Err("task run has no worktree".to_owned())
            } else {
                Ok(Some(mat))
            }
        })
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
        *self.lease_requests.lock().unwrap() += 1;
        *self.lease_acquisitions.lock().unwrap() += 1;
        Box::pin(async { Ok(Box::new(PodProbeLease) as Box<dyn FinalVerificationInvocationLease>) })
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
        panic!("build_mcp_state not needed in pod-shaped tests")
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

/// Build a k8s-shaped fixture: task_run with `workspace_path = NULL`, exactly
/// as djinn-k8s dispatch creates it.
async fn pod_rig(
    probe: PodProbe,
) -> (
    SlotContext,
    FinalVerificationCoordinatorRequest,
    Arc<PodProbe>,
) {
    let db = crate::test_helpers::create_test_db();
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let run = uuid::Uuid::now_v7().to_string();
    // workspace_path = NULL matches djinn-k8s dispatch (pod runs never populate it).
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
    (ctx, request, probe)
}

/// Assert zero lookup (beyond the single plan_unconfigured), zero lease, zero
/// execution, and zero verify-run writes for an unconfigured pod completion.
async fn assert_unconfigured_pod_counters(
    ctx: &SlotContext,
    request: &FinalVerificationCoordinatorRequest,
    probe: &PodProbe,
) {
    assert_eq!(
        probe.consultation_outcomes.lock().unwrap().clone(),
        vec![("disabled", "plan_unconfigured")],
        "exactly one disabled/plan_unconfigured audit outcome"
    );
    assert_eq!(
        *probe.lease_requests.lock().unwrap(),
        0,
        "zero lease requests"
    );
    assert_eq!(
        *probe.lease_acquisitions.lock().unwrap(),
        0,
        "zero lease acquisitions"
    );
    assert_eq!(
        *probe.canonical_executions.lock().unwrap(),
        0,
        "zero canonical command executions"
    );
    assert!(
        recorded_rows(ctx, &request.task_run_id).await.is_empty(),
        "no verify-run row may be written"
    );
}

/// Pod-shaped model-called submit_work: an unconfigured plan with
/// workspace_path = NULL must resolve before consultation and finalize through
/// the legacy skip.
#[tokio::test]
async fn pod_unconfigured_plan_model_submit_work_finalizes_via_legacy_skip() {
    let (ctx, request, probe) = pod_rig(PodProbe::unconfigured()).await;
    let mut intent = crate::output_parser::CompletionIntent {
        finalize_payload: serde_json::json!({
            "task_id": request.task_id,
            "commit_title": "legacy unconfigured submit",
            "summary": "finalize through the typed legacy skip",
            "files_changed": [],
            "remaining_concerns": []
        }),
        tool_use_id: "finalize-tool-use".to_owned(),
        final_verification_evidence: None,
        final_verification_disposition: crate::output_parser::FinalVerificationDisposition::Pending,
    };
    let proceed = verify_completion_intent(
        &mut intent,
        &request.task_id,
        Some(&request.task_run_id),
        CancellationToken::new(),
        &ctx,
        "submit_work",
    )
    .await;
    assert!(matches!(proceed, Ok(None)));
    assert_eq!(
        intent.final_verification_disposition,
        crate::output_parser::FinalVerificationDisposition::NotConfigured
    );
    assert!(
        crate::finalize_handlers::process_completion_intent_with_outcome(
            &intent,
            "submit_work",
            &request.task_id,
            &ctx
        )
        .await,
        "the actual model-submit finalizer must consume the typed skip"
    );
    assert_eq!(probe.events.lock().unwrap().clone(), vec!["resolve"]);
    let activity = djinn_db::TaskRepository::new(ctx.db.clone(), ctx.event_bus.clone())
        .list_activity(&request.task_id)
        .await
        .unwrap();
    let submitted = activity
        .iter()
        .find(|entry| entry.event_type == "work_submitted")
        .expect("model submit must write work_submitted");
    let payload: serde_json::Value = serde_json::from_str(&submitted.payload).unwrap();
    assert_eq!(
        payload["final_verification_evidence"],
        serde_json::Value::Null
    );
    assert_unconfigured_pod_counters(&ctx, &request, &probe).await;
}

/// Pod-shaped auto-submit: an unconfigured plan with workspace_path = NULL must
/// finalize through the legacy skip when the lifecycle generates an auto-submit
/// settlement.
#[tokio::test]
async fn pod_unconfigured_plan_auto_submit_finalizes_via_legacy_skip() {
    use crate::lifecycle::teardown::AutoSubmitSettlementOutcome;
    use djinn_core::auto_submit_decision::{
        AutoSubmitDecision, ReviewAutoSubmitDecisionEvent, VerifyFreshnessEvaluatedEvent,
    };
    use djinn_core::canonical_verify::FreshnessVerdict;
    use djinn_core::models::{AutoSubmitTriggerReason, VerifyRunRecord};
    let (ctx, request, probe) = pod_rig(PodProbe::unconfigured()).await;
    // Build an eligible auto-submit settlement for this task run.
    let settlement = crate::output_parser::AutoSubmitSettlement {
        task_run_id: request.task_run_id.clone(),
        decision: AutoSubmitDecision {
            eligible: true,
            trigger_reason: AutoSubmitTriggerReason::Idle,
            block_reason: None,
            freshness_verdict: FreshnessVerdict::accept(),
        },
        freshness_event: VerifyFreshnessEvaluatedEvent {
            diff_fingerprint: "diff-123".to_string(),
            has_verify_run: true,
            freshness_verdict: FreshnessVerdict::accept(),
            trigger_reason: AutoSubmitTriggerReason::Idle,
            submit_id: None,
        },
        review_event: ReviewAutoSubmitDecisionEvent {
            eligible: true,
            trigger_reason: AutoSubmitTriggerReason::Idle,
            block_reason: None,
            diff_fingerprint: "diff-123".to_string(),
            freshness_verdict: FreshnessVerdict::accept(),
            submit_id: None,
            session_id: Some("session-1".to_string()),
            model_id: Some("model-1".to_string()),
            no_progress_streak: 0,
            model_called_submit_work: false,
        },
        verify_run: Some(VerifyRunRecord {
            id: "verify-1".to_string(),
            task_run_id: request.task_run_id.clone(),
            verify_source: "ci".to_string(),
            verify_run_id: "ci-run-1".to_string(),
            completed_at: "2026-07-01T00:00:00.000Z".to_string(),
            result: "pass".to_string(),
            diff_fingerprint: "diff-123".to_string(),
            created_at: "2026-07-01T00:00:01.000Z".to_string(),
            ..VerifyRunRecord::default()
        }),
        commit_title: Some("auto-submit title".to_string()),
        summary: Some("auto-submit summary".to_string()),
        files_changed: vec!["src/lib.rs".to_string()],
        remaining_concerns: vec![],
    };
    let mut output = crate::output_parser::ParsedAgentOutput::empty();
    output.auto_submit = Some(settlement);
    let task_id = request.task_id.clone();
    let outcome =
        crate::lifecycle::teardown::settle_auto_submit_if_eligible(&task_id, &ctx, &output).await;
    assert_eq!(
        outcome,
        AutoSubmitSettlementOutcome::Submitted,
        "auto-submit for an unconfigured pod plan must finalize through the legacy skip"
    );
    assert_unconfigured_pod_counters(&ctx, &request, &probe).await;
}

/// Pod-shaped controlled termination: an unconfigured plan with
/// workspace_path = NULL must finalize through the legacy skip when the
/// lifecycle generates a controlled-termination auto-submit.
#[tokio::test]
async fn pod_unconfigured_plan_controlled_termination_finalizes_via_legacy_skip() {
    use crate::lifecycle::teardown::AutoSubmitSettlementOutcome;
    use djinn_core::auto_submit_decision::{
        AutoSubmitDecision, ReviewAutoSubmitDecisionEvent, VerifyFreshnessEvaluatedEvent,
    };
    use djinn_core::canonical_verify::FreshnessVerdict;
    use djinn_core::models::{AutoSubmitTriggerReason, VerifyRunRecord};
    let (ctx, request, probe) = pod_rig(PodProbe::unconfigured()).await;
    let settlement = crate::output_parser::AutoSubmitSettlement {
        task_run_id: request.task_run_id.clone(),
        decision: AutoSubmitDecision {
            eligible: true,
            trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
            block_reason: None,
            freshness_verdict: FreshnessVerdict::accept(),
        },
        freshness_event: VerifyFreshnessEvaluatedEvent {
            diff_fingerprint: "diff-123".to_string(),
            has_verify_run: true,
            freshness_verdict: FreshnessVerdict::accept(),
            trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
            submit_id: None,
        },
        review_event: ReviewAutoSubmitDecisionEvent {
            eligible: true,
            trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
            block_reason: None,
            diff_fingerprint: "diff-123".to_string(),
            freshness_verdict: FreshnessVerdict::accept(),
            submit_id: None,
            session_id: Some("session-1".to_string()),
            model_id: Some("model-1".to_string()),
            no_progress_streak: 0,
            model_called_submit_work: false,
        },
        verify_run: Some(VerifyRunRecord {
            id: "verify-1".to_string(),
            task_run_id: request.task_run_id.clone(),
            verify_source: "ci".to_string(),
            verify_run_id: "ci-run-1".to_string(),
            completed_at: "2026-07-01T00:00:00.000Z".to_string(),
            result: "pass".to_string(),
            diff_fingerprint: "diff-123".to_string(),
            created_at: "2026-07-01T00:00:01.000Z".to_string(),
            ..VerifyRunRecord::default()
        }),
        commit_title: Some("controlled-termination title".to_string()),
        summary: Some("controlled-termination summary".to_string()),
        files_changed: vec!["src/lib.rs".to_string()],
        remaining_concerns: vec![],
    };
    let mut output = crate::output_parser::ParsedAgentOutput::empty();
    output.auto_submit = Some(settlement);
    let task_id = request.task_id.clone();
    let outcome =
        crate::lifecycle::teardown::settle_auto_submit_if_eligible(&task_id, &ctx, &output).await;
    assert_eq!(
        outcome,
        AutoSubmitSettlementOutcome::Submitted,
        "controlled termination for an unconfigured pod plan must finalize through the legacy skip"
    );
    assert_unconfigured_pod_counters(&ctx, &request, &probe).await;
}

/// Configured-plan control: a pod with a configured plan but no persisted
/// workspace (workspace_path = NULL) must remain fail-closed — the host
/// resolver errors on the missing worktree, and no completion side effects
/// occur. This proves the plan resolution order does not weaken configured-plan
/// fail-closed semantics.
#[tokio::test]
async fn pod_configured_plan_with_no_workspace_remains_fail_closed() {
    let (ctx, request, probe) = pod_rig(PodProbe::configured_no_worktree()).await;
    let outcome = coordinate_final_verification(request.clone(), &ctx).await;
    assert!(
        matches!(outcome, FinalVerificationRecordingOutcome::Error { .. }),
        "configured plan with no workspace must error (fail-closed), got {outcome:?}"
    );
    // The plan was resolved (configured) but the host returned a resolution
    // error for the missing worktree. No consultation outcome was emitted
    // because the plan resolution failed before reaching consultation.
    assert!(
        probe.consultation_outcomes.lock().unwrap().is_empty(),
        "no lookup outcome may be emitted for a configured plan that fails resolution"
    );
    assert_eq!(
        *probe.lease_requests.lock().unwrap(),
        0,
        "zero lease requests for a resolution failure"
    );
    assert_eq!(
        *probe.canonical_executions.lock().unwrap(),
        0,
        "zero canonical executions for a resolution failure"
    );
    assert!(
        recorded_rows(&ctx, &request.task_run_id).await.is_empty(),
        "no verify-run row for a resolution failure"
    );
}
