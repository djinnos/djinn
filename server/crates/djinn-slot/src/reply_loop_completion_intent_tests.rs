//! Focused tests for the model-called `submit_work` → `CompletionIntent` cutover.
//!
//! These tests exercise the completion-intent coordinator boundary in the reply
//! loop. The host callback is mocked to control the coordinator outcome so every
//! branch (stored, ineligible, error) is testable deterministically without
//! requiring the production hermetic launcher.
//!
//! Repository-backed reuse consultation is covered separately; this module
//! verifies that both stored and reused success evidence survive the reply-loop
//! completion-intent boundary unchanged.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::final_verification::{
    FinalVerificationConsultationFailure, FinalVerificationCoordinatorRequest,
    FinalVerificationRecordingOutcome, FinalVerificationResolvedMaterial,
    FinalVerificationSuccessEvidence,
};
use crate::host::{ResolvedMcpTools, SlotContext, SlotHostCallbacks};
use crate::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::test_helpers::{
    FakeProvider, agent_context_from_db_with_callbacks, create_test_db, create_test_epic,
    create_test_project, create_test_task, test_path, test_tempdir,
};
use djinn_core::{
    canonical_verify::{
        CanonicalCommandDescriptorV1, CanonicalFinalVerificationPlanV1, CanonicalHermeticityV1,
        EnvironmentIdentityV1, ImmutableImageV1, ResolvedEnvironmentIdentityInputV1,
        ToolProbeStatus, ToolProbeV1,
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
    VerificationInputDigestV1, VerificationInputFingerprint, VerificationInputFingerprintConfig,
    compute_verification_input_fingerprint_with_config, run_git_command_in,
};
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::StreamEvent;
use djinn_sandbox::final_verification_execution::{
    FinalVerificationCommandEvidence, FinalVerificationExecutionEvidence,
    FinalVerificationExecutionRequest,
};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Mock host callbacks for completion-intent tests
// ---------------------------------------------------------------------------

/// A mock `SlotHostCallbacks` that supplies final typed outcomes at the
/// coordinator's execution/persistence boundary. It deliberately implements no
/// resolution, lease, sandbox, persistence, or verify-run reuse behavior.
pub(crate) struct CompletionIntentCallbacks {
    outcomes: Mutex<VecDeque<FinalVerificationRecordingOutcome>>,
    coordinator_calls: Mutex<usize>,
    expected_task_id: String,
    reuse_probe: Option<ReuseProbe>,
}

pub(crate) fn fallback_evidence(
    material: &FinalVerificationResolvedMaterial,
    fingerprint: String,
    identity: EnvironmentIdentityV1,
) -> FinalVerificationExecutionEvidence {
    let digest = VerificationInputDigestV1 {
        version: 1,
        fingerprint,
        canonical_stream_len: 1,
        merge_base: Some("main".into()),
        head: "fresh".into(),
        tracked_entry_count: 1,
        extra_entry_count: 0,
    };
    FinalVerificationExecutionEvidence {
        manifest_version: 1,
        pre_environment_identity: Some(identity.clone()),
        post_environment_identity: Some(identity),
        fingerprint_f0: Some(digest.clone()),
        fingerprint_f1: Some(digest),
        commands: material
            .required_checks
            .iter()
            .map(|check_id| FinalVerificationCommandEvidence {
                descriptor: CanonicalCommandDescriptorV1 {
                    check_id: check_id.clone(),
                    executable: format!("{check_id}-tool"),
                    argv: vec![check_id.clone()],
                    working_directory: ".".into(),
                    environment_names: vec![],
                    timeout_seconds: 60,
                    descriptor_revision: 1,
                },
                started_at_unix_millis: 10,
                completed_at_unix_millis: 20,
                exit_code: Some(0),
                timed_out: false,
            })
            .collect(),
        eligibility_reason: None,
    }
}

#[tokio::test]
async fn reply_loop_reuse_rejection_matrix_writes_fresh_authoritative_evidence() {
    // These are deliberately table rows, rather than outcome injection: every
    // row enters consult_reusable_final_verification and falls through to the
    // existing coordinator writer seams.
    for (name, enabled, mutate_c1, failure, expected_error_reason) in [
        ("no-compatible-row", true, false, None, None),
        ("disabled-gate", false, false, None, None),
        ("stale-row", true, false, None, None),
        ("required-coverage-mismatch", true, false, None, None),
        ("manifest-version-mismatch", true, false, None, None),
        ("c1-mutation", true, true, None, None),
        (
            "lookup-failure",
            true,
            false,
            Some(FinalVerificationConsultationFailure::Lookup),
            Some("lookup"),
        ),
        (
            "evaluator-failure",
            true,
            false,
            Some(FinalVerificationConsultationFailure::Evaluator),
            Some("evaluator"),
        ),
        (
            "context-failure",
            true,
            false,
            Some(FinalVerificationConsultationFailure::Context),
            Some("task_context"),
        ),
        (
            "fingerprint-failure",
            true,
            false,
            Some(FinalVerificationConsultationFailure::Fingerprint),
            Some("c0_fingerprint"),
        ),
        (
            "identity-failure",
            true,
            false,
            Some(FinalVerificationConsultationFailure::Identity),
            Some("c0_identity"),
        ),
        (
            "database-failure",
            true,
            false,
            Some(FinalVerificationConsultationFailure::Database),
            Some("gate_database"),
        ),
    ] {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let tree = test_tempdir(&format!("reply-loop-fallback-{name}-"));
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
                .await
                .unwrap();
        }
        std::fs::write(tree.path().join("authored.txt"), name).unwrap();
        for args in [
            vec!["add", "."],
            vec!["commit", "-m", "authored"],
            vec!["branch", "-M", "main"],
        ] {
            run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
                .await
                .unwrap();
        }
        let material = reuse_material(tree.path().to_path_buf());
        let fingerprint = match compute_verification_input_fingerprint_with_config(
            tree.path(),
            &material.execution_request.fingerprint_config,
        )
        .await
        .unwrap()
        {
            VerificationInputFingerprint::Available(value) => value.fingerprint,
            VerificationInputFingerprint::Unavailable(reason) => {
                panic!("fingerprint unavailable: {reason}")
            }
        };
        let identity = EnvironmentIdentityV1::derive(
            (material.execution_request.resolve_environment_identity)().unwrap(),
        )
        .unwrap();
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
        if enabled {
            SettingsRepository::new(db.clone(), crate::test_helpers::test_events())
                .set(
                    &format!("project.{}.verify_run_reuse_enabled", project.id),
                    "true",
                )
                .await
                .unwrap();
        }
        // Every rejection other than the true miss/disabled gate gets a durable,
        // deliberately identifiable candidate.  The writer assertions below then
        // prove that this candidate cannot leak into finalization.
        let mut candidate_id = None;
        if !matches!(name, "no-compatible-row" | "disabled-gate") {
            let seeded_candidate_id = uuid::Uuid::now_v7().to_string();
            let candidate_commands = serde_json::json!([
                {"descriptor_id":"format","result":"pass","passed":true},
                {"descriptor_id":"slot-clippy","result":"pass","passed":true}
            ]);
            let candidate_coverage = if name == "required-coverage-mismatch" {
                serde_json::json!(["format", "slot-clippy", "unexpected"])
            } else {
                serde_json::json!(["format", "slot-clippy"])
            };
            let candidate_manifest = if name == "manifest-version-mismatch" {
                "manifest-v999"
            } else {
                "manifest-v1"
            };
            let candidate_diff = if name == "stale-row" {
                "deliberately-stale"
            } else {
                &material.diff_fingerprint
            };
            let candidate_required = [
                RequiredFinalVerificationCommand {
                    descriptor_id: "format",
                },
                RequiredFinalVerificationCommand {
                    descriptor_id: "slot-clippy",
                },
            ];
            let identity_json = serde_json::from_str(&identity.canonical_json).unwrap();
            VerifyRunRepository::new(db.clone())
                .record_eligible_final_verification_pass(
                    RecordEligibleFinalVerificationPassParams {
                        id: &seeded_candidate_id,
                        task_run_id: &run_id,
                        verify_source: "worker",
                        verify_run_id: "candidate-run",
                        verification_attempt_id: "candidate-attempt",
                        required_commands: &candidate_required,
                        ordered_commands: &candidate_commands,
                        covered_checks: &candidate_coverage,
                        required_checks: &material.required_checks,
                        verification_input_fingerprint: &fingerprint,
                        manifest_version: candidate_manifest,
                        environment_identity_json: &identity_json,
                        environment_identity_digest: &identity.digest,
                        environment_identity_version: "identity-v1",
                        completed_at: "2000-01-01T00:00:00Z",
                        diff_fingerprint: candidate_diff,
                    },
                )
                .await
                .unwrap();
            candidate_id = Some(seeded_candidate_id);
        }
        let callbacks = Arc::new(CompletionIntentCallbacks::for_reuse_with_evidence(
            task.id.clone(),
            material.clone(),
            Some(fallback_evidence(
                &material,
                fingerprint.clone(),
                identity.clone(),
            )),
            mutate_c1,
            failure,
        ));
        let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
        let session = SessionRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone())
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "synthetic/test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(&run_id),
                pricing: None,
                cost_basis: None,
            })
            .await
            .unwrap();
        let provider =
            FakeProvider::script(vec![submit_turn(&format!("submit-{name}"), &task.id, name)]);
        let mut conversation = base_conversation();
        let cancel = CancellationToken::new();
        let (result, output, _, _, _, _) = run_with_provider(
            &provider,
            &[dummy_tool_schema("submit_work")],
            &mut conversation,
            &slot_ctx,
            tree.path().to_str().unwrap(),
            &task.id,
            &session.id,
            &cancel,
        )
        .await;
        assert!(result.is_ok(), "{name}");
        assert_eq!(callbacks.coordinator_count(), 1, "{name}");
        let probe = callbacks.reuse_probe.as_ref().unwrap();
        if let Some(reason) = expected_error_reason {
            assert_eq!(
                *probe.consultation_outcomes.lock().unwrap(),
                vec![("error", reason)],
                "{name}"
            );
        }
        assert_eq!(
            callbacks
                .reuse_events()
                .iter()
                .filter(|event| **event == "writer-resolution")
                .count(),
            1,
            "{name}"
        );
        assert_eq!(*probe.lease_requests.lock().unwrap(), 1, "{name}");
        assert_eq!(*probe.lease_acquisitions.lock().unwrap(), 1, "{name}");
        assert_eq!(*probe.canonical_executions.lock().unwrap(), 1, "{name}");
        let evidence = output
            .completion_intent
            .unwrap()
            .final_verification_evidence
            .unwrap();
        if let Some(candidate_id) = candidate_id.as_deref() {
            assert_ne!(
                evidence.persisted_run_id, candidate_id,
                "{name}: candidate evidence reached finalization"
            );
        }
        let verify_runs = VerifyRunRepository::new(slot_ctx.db.clone())
            .list_for_task_run(&run_id)
            .await
            .unwrap();
        assert_eq!(
            verify_runs
                .iter()
                .filter(|row| candidate_id.as_deref() != Some(row.id.as_str()))
                .count(),
            1,
            "{name}: exactly one fresh authoritative pass must be persisted"
        );
        let stored = VerifyRunRepository::new(slot_ctx.db.clone())
            .get(&evidence.persisted_run_id)
            .await
            .unwrap()
            .expect("fresh finalization evidence names a durable row");
        assert_eq!(stored.id, evidence.persisted_run_id, "{name}");
        assert_eq!(
            stored.verification_input_fingerprint.as_deref(),
            Some(fingerprint.as_str()),
            "{name}"
        );
        assert_eq!(
            stored.manifest_version.as_deref(),
            Some("manifest-v1"),
            "{name}"
        );
        assert_eq!(
            stored.environment_identity_digest.as_deref(),
            Some(identity.digest.as_str()),
            "{name}"
        );
        assert_eq!(
            stored.ordered_commands.clone(),
            Some(serde_json::json!([
                {"descriptor_id":"format","result":"pass","passed":true,"started_at_unix_millis":10,"completed_at_unix_millis":20},
                {"descriptor_id":"slot-clippy","result":"pass","passed":true,"started_at_unix_millis":10,"completed_at_unix_millis":20}
            ])),
            "{name}"
        );
        assert_eq!(
            evidence.verification_input_fingerprint, fingerprint,
            "{name}"
        );
        assert_eq!(evidence.manifest_version, "manifest-v1", "{name}");
        assert_eq!(
            evidence.environment_identity_digest, identity.digest,
            "{name}"
        );
        assert_eq!(
            &evidence.covered_checks,
            stored
                .covered_checks
                .as_ref()
                .expect("fresh durable evidence records covered checks"),
            "{name}"
        );
        assert_eq!(
            evidence.required_checks,
            vec!["format", "slot-clippy"],
            "{name}"
        );
        assert_eq!(
            evidence.covered_checks,
            serde_json::json!(["format", "slot-clippy"]),
            "{name}"
        );
        assert_eq!(
            evidence.ordered_commands,
            stored.ordered_commands.unwrap(),
            "{name}"
        );
        assert!(error_ids(&conversation).is_empty(), "{name}");
    }
}

pub(crate) fn reuse_material(worktree: std::path::PathBuf) -> FinalVerificationResolvedMaterial {
    reuse_material_with_fingerprint_config(worktree, VerificationInputFingerprintConfig::default())
}

/// Construct the same production resolved material used by the reuse fixtures,
/// retaining a caller-provided complete input configuration. Boundary tests use
/// this to ensure C2 resolution observes external inputs as well as worktree
/// inputs instead of comparing a separately computed test-only digest.
pub(crate) fn reuse_material_with_fingerprint_config(
    worktree: std::path::PathBuf,
    fingerprint_config: VerificationInputFingerprintConfig,
) -> FinalVerificationResolvedMaterial {
    let required_checks = vec!["format".to_owned(), "slot-clippy".to_owned()];
    let manifest = fingerprint_config.manifest.clone();
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
            profile_id: "reply-loop-reuse".into(),
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
            fingerprint_config,
            tool_runtime: vec![],
            read_only_external_mounts: vec![],
            output_directories: vec![],
        },
        verify_source: VerifySource::Worker,
        required_checks,
        diff_fingerprint: "reply-loop-reuse-audit-diff".into(),
    }
}

struct ReuseProbe {
    material: FinalVerificationResolvedMaterial,
    events: Mutex<Vec<&'static str>>,
    lease_requests: Mutex<usize>,
    lease_acquisitions: Mutex<usize>,
    canonical_executions: Mutex<usize>,
    resolved_fingerprints: Mutex<Vec<String>>,
    evidence: Mutex<Option<FinalVerificationExecutionEvidence>>,
    mutate_before_c1: bool,
    failure: Option<FinalVerificationConsultationFailure>,
    require_workspace_path: bool,
    consultation_outcomes: Mutex<Vec<(&'static str, &'static str)>>,
}

struct ReuseProbeLease;

impl crate::final_verification::FinalVerificationInvocationLease for ReuseProbeLease {
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

impl CompletionIntentCallbacks {
    fn new(expected_task_id: String, outcomes: Vec<FinalVerificationRecordingOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into()),
            coordinator_calls: Mutex::new(0),
            expected_task_id,
            reuse_probe: None,
        }
    }

    fn for_reuse(expected_task_id: String, material: FinalVerificationResolvedMaterial) -> Self {
        Self::for_reuse_with_evidence(expected_task_id, material, None, false, None)
    }

    pub(crate) fn for_reuse_with_evidence(
        expected_task_id: String,
        material: FinalVerificationResolvedMaterial,
        evidence: Option<FinalVerificationExecutionEvidence>,
        mutate_before_c1: bool,
        failure: Option<FinalVerificationConsultationFailure>,
    ) -> Self {
        Self {
            outcomes: Mutex::new(VecDeque::new()),
            coordinator_calls: Mutex::new(0),
            expected_task_id,
            reuse_probe: Some(ReuseProbe {
                material,
                events: Mutex::new(Vec::new()),
                lease_requests: Mutex::new(0),
                lease_acquisitions: Mutex::new(0),
                canonical_executions: Mutex::new(0),
                resolved_fingerprints: Mutex::new(Vec::new()),
                evidence: Mutex::new(evidence),
                mutate_before_c1,
                failure,
                require_workspace_path: false,
                consultation_outcomes: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Model the configured production resolver's task-run worktree precondition
    /// while retaining the reusable material/executor probe used by C2 tests.
    pub(crate) fn for_reuse_requiring_workspace(
        expected_task_id: String,
        material: FinalVerificationResolvedMaterial,
    ) -> Self {
        let mut callbacks =
            Self::for_reuse_with_evidence(expected_task_id, material, None, false, None);
        callbacks
            .reuse_probe
            .as_mut()
            .expect("reuse probe")
            .require_workspace_path = true;
        callbacks
    }

    fn coordinator_count(&self) -> usize {
        *self.coordinator_calls.lock().unwrap()
    }

    pub(crate) fn reuse_events(&self) -> Vec<&'static str> {
        self.reuse_probe
            .as_ref()
            .unwrap()
            .events
            .lock()
            .unwrap()
            .clone()
    }

    pub(crate) fn resolved_fingerprints(&self) -> Vec<String> {
        self.reuse_probe
            .as_ref()
            .unwrap()
            .resolved_fingerprints
            .lock()
            .unwrap()
            .clone()
    }
}

impl SlotHostCallbacks for CompletionIntentCallbacks {
    fn inject_final_verification_consultation_failure_for_test(
        &self,
        failure: FinalVerificationConsultationFailure,
    ) -> bool {
        self.reuse_probe.as_ref().and_then(|probe| probe.failure) == Some(failure)
    }
    fn record_final_verification_consultation_outcome_for_test(
        &self,
        outcome: &'static str,
        reason: &'static str,
    ) {
        self.reuse_probe
            .as_ref()
            .unwrap()
            .consultation_outcomes
            .lock()
            .unwrap()
            .push((outcome, reason));
    }

    fn final_verification_outcome_for_test(
        &self,
        request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationRecordingOutcome> {
        assert_eq!(
            request.task_id, self.expected_task_id,
            "fixture task ID reached completion-intent verification"
        );
        *self.coordinator_calls.lock().unwrap() += 1;
        if let Some(probe) = &self.reuse_probe {
            // Coordinator entry follows parsing and acceptance of submit_work.
            // Returning None intentionally leaves production consultation live.
            probe
                .events
                .lock()
                .unwrap()
                .push("completion-intent-accepted");
            return None;
        }
        self.outcomes.lock().unwrap().pop_front()
    }

    fn final_verification_evidence_for_test(
        &self,
        _request: &FinalVerificationCoordinatorRequest,
    ) -> Option<djinn_sandbox::final_verification_execution::FinalVerificationExecutionEvidence>
    {
        if let Some(probe) = &self.reuse_probe {
            *probe.canonical_executions.lock().unwrap() += 1;
            probe.events.lock().unwrap().push("canonical-execution");
            return probe.evidence.lock().unwrap().clone();
        }
        None
    }

    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        verify_run_id: &'a str,
        ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<FinalVerificationResolvedMaterial>, String>>
                + Send
                + 'a,
        >,
    > {
        let probe = self.reuse_probe.as_ref();
        Box::pin(async move {
            let probe = probe.ok_or_else(|| "not implemented in test".to_owned())?;
            probe.events.lock().unwrap().push(match verify_run_id {
                "reuse-c0" => "consult-reuse-c0",
                "reuse-c1" => "consult-reuse-c1",
                _ => "writer-resolution",
            });
            if verify_run_id == "reuse-c1" && probe.mutate_before_c1 {
                std::fs::write(
                    probe
                        .material
                        .execution_request
                        .worktree
                        .join("c1-mutation"),
                    "changed",
                )
                .map_err(|error| error.to_string())?;
            }
            if probe.require_workspace_path {
                let task_run = TaskRunRepository::new(ctx.db.clone())
                    .get(task_run_id)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| {
                        "task run disappeared during configured resolution".to_owned()
                    })?;
                if task_run.workspace_path.is_none() {
                    return Err("task run has no worktree".to_owned());
                }
            }
            if verify_run_id != "reuse-c0" && verify_run_id != "reuse-c1" {
                let fingerprint = match compute_verification_input_fingerprint_with_config(
                    &probe.material.execution_request.worktree,
                    &probe.material.execution_request.fingerprint_config,
                )
                .await
                .map_err(|error| error.to_string())?
                {
                    VerificationInputFingerprint::Available(value) => value.fingerprint,
                    VerificationInputFingerprint::Unavailable(reason) => {
                        return Err(reason.to_string());
                    }
                };
                probe
                    .resolved_fingerprints
                    .lock()
                    .unwrap()
                    .push(fingerprint);
            }
            Ok(Some(probe.material.clone()))
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
            dyn Future<
                    Output = Result<
                        Box<dyn crate::final_verification::FinalVerificationInvocationLease>,
                        String,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let probe = self.reuse_probe.as_ref();
        if let Some(probe) = probe {
            *probe.lease_requests.lock().unwrap() += 1;
        }
        Box::pin(async move {
            let probe = probe.ok_or_else(|| "not implemented in test".to_owned())?;
            *probe.lease_acquisitions.lock().unwrap() += 1;
            Ok(Box::new(ReuseProbeLease)
                as Box<
                    dyn crate::final_verification::FinalVerificationInvocationLease,
                >)
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
        panic!("build_mcp_state not needed in completion-intent tests")
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
        Box::pin(async { Err("not implemented".into()) })
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
// Test fixtures
// ---------------------------------------------------------------------------

fn dummy_tool_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} },
        "concurrent_safe": false
    })
}

fn base_conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a worker."));
    conversation.push(Message::user("Do the task."));
    conversation
}

struct TestFixture {
    slot_ctx: SlotContext,
    project_path: String,
    task_id: String,
    session_id: String,
    cancel: CancellationToken,
    callbacks: Arc<CompletionIntentCallbacks>,
}

async fn make_fixture(outcomes: Vec<FinalVerificationRecordingOutcome>) -> TestFixture {
    let cancel = CancellationToken::new();
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
        .to_string_lossy()
        .into_owned();

    // Create an active task run so `verify_completion_intent` can find it.
    let run_id = uuid::Uuid::now_v7().to_string();
    let worktree = test_path("djinn-completion-intent-");
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some(worktree.to_str().unwrap()),
            mirror_ref: None,
        })
        .await
        .expect("create task run");

    let callbacks = Arc::new(CompletionIntentCallbacks::new(task.id.clone(), outcomes));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    let session = SessionRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone())
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "synthetic/test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create completion-intent test session");

    TestFixture {
        slot_ctx,
        project_path,
        task_id: task.id,
        session_id: session.id,
        cancel,
        callbacks,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_with_provider(
    provider: &dyn djinn_provider::provider::LlmProvider,
    tools: &[serde_json::Value],
    conversation: &mut Conversation,
    slot_ctx: &SlotContext,
    project_path: &str,
    task_id: &str,
    session_id: &str,
    cancel: &CancellationToken,
) -> (
    anyhow::Result<()>,
    crate::output_parser::ParsedAgentOutput,
    i64,
    i64,
    i64,
    i64,
) {
    let worktree = test_path("djinn-reply-loop-ci-");
    let worktree_path = worktree.as_path();
    run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider,
            tools,
            task_id,
            task_short_id: "t1",
            session_id,
            project_path,
            worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "synthetic/test-model",
            cancel,
            global_cancel: cancel,
            ctx: slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        conversation,
        false,
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn stored_evidence() -> FinalVerificationSuccessEvidence {
    FinalVerificationSuccessEvidence {
        persisted_run_id: "persisted-stored".into(),
        completed_at: "2025-01-01T00:00:00Z".into(),
        ordered_commands: serde_json::json!([
            {"command": "cargo fmt --check", "ordinal": 0},
            {"command": "cargo test -p djinn-slot", "ordinal": 1}
        ]),
        covered_checks: serde_json::json!(["format", "slot-tests"]),
        required_checks: vec!["format".into(), "slot-tests".into()],
        verification_input_fingerprint: "stored-complete-fingerprint".into(),
        manifest_version: "manifest-v1".into(),
        environment_identity_digest: "stored-environment-digest".into(),
    }
}

fn stored() -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Stored {
        verification_attempt_id: "attempt-stored".into(),
        verify_run_id: "run-stored".into(),
        evidence: Box::new(stored_evidence()),
    }
}

fn ineligible(reason: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Ineligible {
        verification_attempt_id: "attempt-ineligible".into(),
        reason: reason.into(),
    }
}

fn coordinator_error(detail: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Error {
        verification_attempt_id: "attempt-error".into(),
        detail: detail.into(),
    }
}

fn submit_turn(id: &str, task_id: &str, summary: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: id.into(),
            name: "submit_work".into(),
            input: serde_json::json!({
                "task_id": task_id,
                "commit_title": format!("complete {summary}"),
                "summary": summary,
            }),
        }),
        StreamEvent::Done,
    ]
}

fn error_ids(conversation: &Conversation) -> Vec<&str> {
    conversation
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error: true,
                ..
            } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn stored_verification_forwards_original_payload_exactly_once() {
    let fixture = make_fixture(vec![stored()]).await;
    let expected = serde_json::json!({
        "task_id": fixture.task_id,
        "commit_title": "complete finished work",
        "summary": "finished work",
    });
    let provider = FakeProvider::script(vec![submit_turn(
        "submit-1",
        &fixture.task_id,
        "finished work",
    )]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(fixture.callbacks.coordinator_count(), 1);
    assert_eq!(output.finalize_payload.as_ref(), Some(&expected));
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    let intent = output
        .completion_intent
        .expect("valid payload reached completion-intent verification");
    assert_eq!(intent.finalize_payload, expected);
    assert_eq!(intent.tool_use_id, "submit-1");
    assert_eq!(intent.final_verification_evidence, Some(stored_evidence()));
    assert!(error_ids(&conversation).is_empty());
}

#[tokio::test]
async fn repeat_worker_reuses_compatible_persisted_pass_after_completion_intent() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let tree = test_tempdir("reply-loop-reuse-hit-");
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Reply Loop Test"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    std::fs::write(
        tree.path().join("authored.txt"),
        "repeat-worker authored state",
    )
    .unwrap();
    for args in [
        vec!["add", "authored.txt"],
        vec!["commit", "-m", "authored state"],
        vec!["branch", "-M", "main"],
    ] {
        run_git_command_in(tree.path(), args.into_iter().map(String::from).collect())
            .await
            .unwrap();
    }
    let material = reuse_material(tree.path().to_path_buf());
    let fingerprint = match compute_verification_input_fingerprint_with_config(
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
    let persisted_id = "persisted-reply-loop-reuse-hit";
    let completed_at = "2099-02-02T03:04:05Z";
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
            task_run_id: &run_id,
            verify_source: "worker",
            verify_run_id: "seeded-reuse-run",
            verification_attempt_id: "seeded-reuse-attempt",
            required_commands: &commands,
            ordered_commands: &ordered_commands,
            covered_checks: &covered_checks,
            required_checks: &material.required_checks,
            verification_input_fingerprint: &fingerprint,
            manifest_version: "manifest-v1",
            environment_identity_json: &identity_json,
            environment_identity_digest: &identity.digest,
            environment_identity_version: "identity-v1",
            completed_at,
            diff_fingerprint: &material.diff_fingerprint,
        })
        .await
        .unwrap();
    let callbacks = Arc::new(CompletionIntentCallbacks::for_reuse(
        task.id.clone(),
        material,
    ));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    let session = SessionRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone())
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "synthetic/test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    let expected_payload = serde_json::json!({
        "task_id": task.id,
        "commit_title": "complete reused verification",
        "summary": "reused verification",
    });
    let provider = FakeProvider::script(vec![submit_turn(
        "submit-reused",
        &task.id,
        "reused verification",
    )]);
    let mut conversation = base_conversation();
    let cancel = CancellationToken::new();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &slot_ctx,
        tree.path().to_str().unwrap(),
        &task.id,
        &session.id,
        &cancel,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(callbacks.coordinator_count(), 1);
    assert_eq!(output.finalize_payload.as_ref(), Some(&expected_payload));
    let intent = output
        .completion_intent
        .expect("reused verification reached finalization");
    assert_eq!(intent.finalize_payload, expected_payload);
    assert_eq!(intent.tool_use_id, "submit-reused");
    assert_eq!(
        callbacks.reuse_events(),
        vec![
            "completion-intent-accepted",
            "consult-reuse-c0",
            "consult-reuse-c1"
        ]
    );
    let evidence = intent
        .final_verification_evidence
        .expect("persisted evidence reaches finalization");
    assert_eq!(evidence.persisted_run_id, persisted_id);
    assert_eq!(evidence.completed_at, completed_at);
    assert_eq!(evidence.ordered_commands, ordered_commands);
    assert_eq!(evidence.covered_checks, covered_checks);
    assert_eq!(evidence.required_checks, vec!["format", "slot-clippy"]);
    assert_eq!(evidence.verification_input_fingerprint, fingerprint);
    assert_eq!(evidence.manifest_version, "manifest-v1");
    assert_eq!(evidence.environment_identity_digest, identity.digest);
    let probe = callbacks.reuse_probe.as_ref().unwrap();
    assert_eq!(*probe.lease_requests.lock().unwrap(), 0);
    assert_eq!(*probe.lease_acquisitions.lock().unwrap(), 0);
    assert_eq!(*probe.canonical_executions.lock().unwrap(), 0);
    assert!(error_ids(&conversation).is_empty());
}

#[tokio::test]
async fn ineligible_result_is_persisted_and_valid_resubmission_is_reverified() {
    let fixture = make_fixture(vec![ineligible("command failed"), stored()]).await;
    let provider = FakeProvider::script(vec![
        submit_turn("submit-failed", &fixture.task_id, "first attempt"),
        submit_turn("submit-stored", &fixture.task_id, "corrected attempt"),
    ]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(fixture.callbacks.coordinator_count(), 2);
    assert_eq!(error_ids(&conversation), vec!["submit-failed"]);
    assert_eq!(
        output.finalize_payload.as_ref().unwrap()["summary"],
        "corrected attempt"
    );
    let persisted = djinn_db::SessionMessageRepository::new(
        fixture.slot_ctx.db.clone(),
        fixture.slot_ctx.event_bus.clone(),
    )
    .load_conversation(&fixture.session_id)
    .await
    .expect("load persisted conversation");
    assert_eq!(error_ids(&persisted), vec!["submit-failed"]);
}

#[tokio::test]
async fn terminal_error_exhausts_conversation_without_success_or_submission() {
    let fixture = make_fixture(vec![coordinator_error("persistence unavailable")]).await;
    let provider = FakeProvider::script_with_terminal_error(
        vec![submit_turn("submit-error", &fixture.task_id, "attempt")],
        "terminal provider failure after submit-error",
    );
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(
        result.is_err(),
        "explicit provider failure terminates the real reply loop"
    );
    assert_eq!(provider.remaining(), 0, "terminal provider turn consumed");
    assert_eq!(fixture.callbacks.coordinator_count(), 1);
    assert!(output.finalize_payload.is_none());
    assert!(output.completion_intent.is_none());
    assert_eq!(error_ids(&conversation), vec!["submit-error"]);
}

#[tokio::test]
async fn three_non_stored_attempts_each_reach_verification_and_never_succeed() {
    let fixture = make_fixture(vec![
        ineligible("command one failed"),
        coordinator_error("writer failed"),
        ineligible("command three failed"),
    ])
    .await;
    let provider = FakeProvider::script_with_terminal_error(
        vec![
            submit_turn("submit-1", &fixture.task_id, "attempt one"),
            submit_turn("submit-2", &fixture.task_id, "attempt two"),
            submit_turn("submit-3", &fixture.task_id, "attempt three"),
        ],
        "terminal provider failure after three non-stored attempts",
    );
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(provider.remaining(), 0, "terminal provider turn consumed");
    assert_eq!(fixture.callbacks.coordinator_count(), 3);
    assert!(output.finalize_payload.is_none());
    assert!(output.completion_intent.is_none());
    assert_eq!(
        error_ids(&conversation),
        vec!["submit-1", "submit-2", "submit-3"]
    );
}
