use std::sync::Arc;

use crate::final_verification::FinalVerificationSuccessEvidence;
use crate::finalize_handlers::{
    process_auto_submit_payload, process_completion_intent_with_outcome,
};
use crate::finalize_handlers_fingerprint_tests::{
    c2_fingerprint, create_k8s_run_then_persist_workspace, create_run_with_workspace,
    init_git_repo_with_dirty_file,
};
use crate::output_parser::CompletionIntent;
use crate::reply_loop_completion_intent_tests::{
    CompletionIntentCallbacks, fallback_evidence, reuse_material_with_fingerprint_config,
};
use crate::test_helpers;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::{CreateTaskAttemptParams, TaskAttemptRepository};
use djinn_git::VerificationInputFingerprintConfig;
use djinn_sandbox::final_verification_execution::FinalVerificationIneligibilityReason;

#[derive(Clone, Copy, Debug)]
enum IdentityCompatibilityMutation {
    OrderedCommandDescriptors,
    ProfileRevision,
    ImmutableImage,
    ImmutableToolchainVersion,
    ImmutableToolchainDigest,
    RunnerVersion,
    Lockfile,
    Features,
    AllowlistedEnvironment,
    ManifestVersion,
    MissingIdentityDigest,
    LegacyIdentityDigest,
    RequiredCoverage,
}

impl IdentityCompatibilityMutation {
    fn name(self) -> &'static str {
        match self {
            Self::OrderedCommandDescriptors => "ordered-command-descriptors",
            Self::ProfileRevision => "profile-revision",
            Self::ImmutableImage => "immutable-image",
            Self::ImmutableToolchainVersion => "immutable-toolchain-version",
            Self::ImmutableToolchainDigest => "immutable-toolchain-digest",
            Self::RunnerVersion => "runner-version",
            Self::Lockfile => "lockfile",
            Self::Features => "features",
            Self::AllowlistedEnvironment => "allowlisted-environment",
            Self::ManifestVersion => "manifest-version",
            Self::MissingIdentityDigest => "missing-identity-digest",
            Self::LegacyIdentityDigest => "legacy-identity-digest",
            Self::RequiredCoverage => "required-coverage-mismatch",
        }
    }

    fn apply(self, input: &mut djinn_core::canonical_verify::ResolvedEnvironmentIdentityInputV1) {
        match self {
            Self::OrderedCommandDescriptors => input.plan.commands.reverse(),
            Self::ProfileRevision => input.plan.profile_revision += 1,
            Self::ImmutableImage => {
                input.image.digest =
                    "sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd".into()
            }
            Self::ImmutableToolchainVersion => input.tool_probes[0].version = "test-next".into(),
            Self::ImmutableToolchainDigest => {
                input.tool_probes[0].executable_digest =
                    "sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd".into()
            }
            Self::RunnerVersion => input.runner_version = "test-runner-next".into(),
            Self::Lockfile => {
                input
                    .lockfile_digests
                    .push(djinn_core::canonical_verify::LockfileDigestV1 {
                    path: "Cargo.lock".into(),
                    digest:
                        "sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                            .into(),
                });
            }
            Self::Features => {
                input.features.push("next-feature".into());
            }
            Self::AllowlistedEnvironment => {
                input
                    .allowlisted_environment
                    .insert("RUSTFLAGS".into(), "-Dwarnings".into());
            }
            Self::ManifestVersion
            | Self::MissingIdentityDigest
            | Self::LegacyIdentityDigest
            | Self::RequiredCoverage => {}
        }
    }

    fn stale_identity_digest(self, persisted: &str) -> String {
        match self {
            Self::MissingIdentityDigest => String::new(),
            Self::LegacyIdentityDigest => "environment-identity-v0".into(),
            _ => persisted.to_owned(),
        }
    }

    fn stale_manifest_version(self, current: u32) -> String {
        match self {
            Self::ManifestVersion => format!("manifest-v{}", current + 1),
            _ => format!("manifest-v{current}"),
        }
    }

    fn stale_covered_checks(self) -> serde_json::Value {
        match self {
            Self::RequiredCoverage => serde_json::json!(["format", "slot-clippy", "unexpected"]),
            _ => serde_json::json!(["format", "slot-clippy"]),
        }
    }
}

fn compatibility_material(
    worktree: std::path::PathBuf,
    mutation: IdentityCompatibilityMutation,
) -> (
    crate::final_verification::FinalVerificationResolvedMaterial,
    djinn_core::canonical_verify::EnvironmentIdentityV1,
    djinn_core::canonical_verify::EnvironmentIdentityV1,
    u32,
) {
    let mut material = reuse_material_with_fingerprint_config(
        worktree,
        VerificationInputFingerprintConfig::default(),
    );
    let baseline = (material.execution_request.resolve_environment_identity)().unwrap();
    let persisted =
        djinn_core::canonical_verify::EnvironmentIdentityV1::derive(baseline.clone()).unwrap();
    let mut current = baseline;
    mutation.apply(&mut current);
    let manifest_version = current.input_manifest.version;
    let identity =
        djinn_core::canonical_verify::EnvironmentIdentityV1::derive(current.clone()).unwrap();
    material.execution_request.resolve_environment_identity = Arc::new(move || Ok(current.clone()));
    (material, persisted, identity, manifest_version)
}

async fn assert_identity_mismatch_rebuilds_current_evidence(
    mutation: IdentityCompatibilityMutation,
) {
    let name = mutation.name();
    let worktree = init_git_repo_with_dirty_file();
    let (material, persisted_identity, current_identity, manifest_version) =
        compatibility_material(worktree.path().to_path_buf(), mutation);
    let fingerprint = c2_fingerprint(
        worktree.path(),
        &material.execution_request.fingerprint_config,
    )
    .await;
    assert_ne!(
        fingerprint, material.diff_fingerprint,
        "{name}: C2 fingerprint is not the submission-diff fingerprint"
    );
    let stale_identity_digest = mutation.stale_identity_digest(&persisted_identity.digest);
    let stale_manifest_version = mutation.stale_manifest_version(manifest_version);
    assert!(
        stale_identity_digest != current_identity.digest
            || stale_manifest_version != format!("manifest-v{manifest_version}")
            || mutation.stale_covered_checks() != serde_json::json!(material.required_checks),
        "{name}: persisted identity or manifest must differ from current evidence"
    );
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    create_k8s_run_then_persist_workspace(
        &db,
        &project.id,
        &task.id,
        worktree.path().to_str().unwrap(),
    )
    .await;
    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &format!("identity-{name}-{}", uuid::Uuid::now_v7()),
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();
    let stale = FinalVerificationSuccessEvidence {
        persisted_run_id: format!("persisted-{name}"),
        completed_at: "2026-01-01T00:00:00Z".into(),
        ordered_commands: serde_json::json!([{"descriptor_id":"format"},{"descriptor_id":"slot-clippy"}]),
        covered_checks: mutation.stale_covered_checks(),
        required_checks: material.required_checks.clone(),
        verification_input_fingerprint: fingerprint.clone(),
        manifest_version: stale_manifest_version,
        environment_identity_digest: stale_identity_digest,
    };
    let intent = CompletionIntent {
        finalize_payload: serde_json::json!({"task_id":task.id,"commit_title":"identity","summary":"identity","files_changed":[],"remaining_concerns":[]}),
        tool_use_id: format!("identity-{name}"),
        final_verification_evidence: Some(stale.clone()),
    };
    let mut failed = fallback_evidence(&material, fingerprint.clone(), current_identity.clone());
    failed.manifest_version = manifest_version;
    failed.commands[0].exit_code = Some(1);
    failed.eligibility_reason = Some(FinalVerificationIneligibilityReason::CommandFailed {
        check_id: failed.commands[0].descriptor.check_id.clone(),
        exit_code: Some(1),
    });
    let failing = Arc::new(CompletionIntentCallbacks::for_reuse_with_evidence(
        task.id.clone(),
        material.clone(),
        Some(failed),
        false,
        None,
    ));
    let failing_ctx =
        test_helpers::agent_context_from_db_with_callbacks(db.clone(), failing.clone());
    assert!(
        !process_completion_intent_with_outcome(&intent, "submit_work", &task.id, &failing_ctx)
            .await,
        "{name}"
    );
    assert!(
        failing.reuse_events().contains(&"canonical-execution"),
        "{name}: must rebuild"
    );
    let activity = djinn_db::TaskRepository::new(db.clone(), failing_ctx.event_bus.clone())
        .list_activity(&task.id)
        .await
        .unwrap();
    assert!(
        activity
            .iter()
            .all(|entry| entry.event_type != "work_submitted"),
        "{name}"
    );
    assert_eq!(
        TaskAttemptRepository::new(db.clone())
            .get(&attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "pending",
        "{name}"
    );

    let mut passing_evidence =
        fallback_evidence(&material, fingerprint.clone(), current_identity.clone());
    passing_evidence.manifest_version = manifest_version;
    let passing = Arc::new(CompletionIntentCallbacks::for_reuse_with_evidence(
        task.id.clone(),
        material.clone(),
        Some(passing_evidence),
        false,
        None,
    ));
    let passing_ctx =
        test_helpers::agent_context_from_db_with_callbacks(db.clone(), passing.clone());
    assert!(
        process_completion_intent_with_outcome(&intent, "submit_work", &task.id, &passing_ctx)
            .await,
        "{name}"
    );
    assert!(
        passing.reuse_events().contains(&"canonical-execution"),
        "{name}: canonical rebuild"
    );
    let activity = djinn_db::TaskRepository::new(db.clone(), passing_ctx.event_bus.clone())
        .list_activity(&task.id)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(
        &activity
            .iter()
            .find(|entry| entry.event_type == "work_submitted")
            .unwrap()
            .payload,
    )
    .unwrap();
    let evidence = &payload["final_verification_evidence"];
    assert_ne!(
        evidence["persisted_run_id"], stale.persisted_run_id,
        "{name}: stale row cannot finalize"
    );
    assert_eq!(
        evidence["verification_input_fingerprint"], fingerprint,
        "{name}"
    );
    assert_eq!(
        evidence["environment_identity_digest"], current_identity.digest,
        "{name}"
    );
    assert_eq!(
        evidence["manifest_version"],
        format!("manifest-v{manifest_version}"),
        "{name}"
    );
    assert_eq!(
        evidence["required_checks"],
        serde_json::json!(["format", "slot-clippy"]),
        "{name}"
    );
    assert_eq!(
        evidence["covered_checks"],
        serde_json::json!(["format", "slot-clippy"]),
        "{name}"
    );
    assert_eq!(
        evidence["ordered_commands"][0]["descriptor_id"], "format",
        "{name}"
    );
    assert_eq!(
        evidence["ordered_commands"][1]["descriptor_id"], "slot-clippy",
        "{name}"
    );
    assert_eq!(
        TaskAttemptRepository::new(db)
            .get(&attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "submitted",
        "{name}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c2_identity_compatibility_matrix_rebuilds_every_mismatch() {
    for mutation in [
        IdentityCompatibilityMutation::OrderedCommandDescriptors,
        IdentityCompatibilityMutation::ProfileRevision,
        IdentityCompatibilityMutation::ImmutableImage,
        IdentityCompatibilityMutation::ImmutableToolchainVersion,
        IdentityCompatibilityMutation::ImmutableToolchainDigest,
        IdentityCompatibilityMutation::RunnerVersion,
        IdentityCompatibilityMutation::Lockfile,
        IdentityCompatibilityMutation::Features,
        IdentityCompatibilityMutation::AllowlistedEnvironment,
        IdentityCompatibilityMutation::ManifestVersion,
        IdentityCompatibilityMutation::MissingIdentityDigest,
        IdentityCompatibilityMutation::LegacyIdentityDigest,
        IdentityCompatibilityMutation::RequiredCoverage,
    ] {
        assert_identity_mismatch_rebuilds_current_evidence(mutation).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c2_fully_compatible_evidence_finalizes_without_canonical_rebuild() {
    let worktree = init_git_repo_with_dirty_file();
    let compatible_material = reuse_material_with_fingerprint_config(
        worktree.path().to_path_buf(),
        VerificationInputFingerprintConfig::default(),
    );
    let current_identity = djinn_core::canonical_verify::EnvironmentIdentityV1::derive(
        (compatible_material
            .execution_request
            .resolve_environment_identity)()
        .unwrap(),
    )
    .unwrap();
    let manifest_version = 1;
    let fingerprint = c2_fingerprint(
        worktree.path(),
        &compatible_material.execution_request.fingerprint_config,
    )
    .await;
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    create_k8s_run_then_persist_workspace(
        &db,
        &project.id,
        &task.id,
        worktree.path().to_str().unwrap(),
    )
    .await;
    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: "compatible-c2",
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();
    let candidate = FinalVerificationSuccessEvidence {
        persisted_run_id: "compatible-c2-run".into(),
        completed_at: "2026-01-01T00:00:00Z".into(),
        ordered_commands: serde_json::json!([{"descriptor_id":"format"},{"descriptor_id":"slot-clippy"}]),
        covered_checks: serde_json::json!(["format", "slot-clippy"]),
        required_checks: compatible_material.required_checks.clone(),
        verification_input_fingerprint: fingerprint.clone(),
        manifest_version: format!("manifest-v{manifest_version}"),
        environment_identity_digest: current_identity.digest.clone(),
    };
    let intent = CompletionIntent {
        finalize_payload: serde_json::json!({"task_id":task.id,"commit_title":"compatible","summary":"compatible","files_changed":[],"remaining_concerns":[]}),
        tool_use_id: "compatible-c2".into(),
        final_verification_evidence: Some(candidate.clone()),
    };
    let callbacks = Arc::new(CompletionIntentCallbacks::for_reuse_with_evidence(
        task.id.clone(),
        compatible_material,
        None,
        false,
        None,
    ));
    let ctx = test_helpers::agent_context_from_db_with_callbacks(db.clone(), callbacks.clone());
    assert!(process_completion_intent_with_outcome(&intent, "submit_work", &task.id, &ctx).await);
    assert!(
        !callbacks.reuse_events().contains(&"canonical-execution"),
        "compatible C2 evidence must be consumed without canonical rebuilding"
    );
    let activity = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone())
        .list_activity(&task.id)
        .await
        .unwrap();
    let submitted = activity
        .iter()
        .find(|entry| entry.event_type == "work_submitted")
        .expect("compatible evidence emits work_submitted");
    let evidence: serde_json::Value = serde_json::from_str::<serde_json::Value>(&submitted.payload)
        .unwrap()["final_verification_evidence"]
        .clone();
    assert_eq!(evidence["persisted_run_id"], candidate.persisted_run_id);
    assert_eq!(evidence["verification_input_fingerprint"], fingerprint);
    assert_eq!(
        evidence["environment_identity_digest"],
        current_identity.digest
    );
    assert_eq!(
        evidence["manifest_version"],
        format!("manifest-v{manifest_version}")
    );
    assert_eq!(evidence["ordered_commands"], candidate.ordered_commands);
    assert_eq!(
        evidence["required_checks"],
        serde_json::json!(["format", "slot-clippy"])
    );
    assert_eq!(
        evidence["covered_checks"],
        serde_json::json!(["format", "slot-clippy"])
    );
    assert_eq!(
        TaskAttemptRepository::new(db)
            .get(&attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "submitted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_submit_stale_identity_failed_reverification_has_no_success_side_effect() {
    let worktree = init_git_repo_with_dirty_file();
    let (material, persisted, current, manifest_version) = compatibility_material(
        worktree.path().to_path_buf(),
        IdentityCompatibilityMutation::RunnerVersion,
    );
    let fingerprint = c2_fingerprint(
        worktree.path(),
        &material.execution_request.fingerprint_config,
    )
    .await;
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    create_k8s_run_then_persist_workspace(
        &db,
        &project.id,
        &task.id,
        worktree.path().to_str().unwrap(),
    )
    .await;
    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: "auto-stale-identity",
            session_id: None,
            attempt_seq: None,
        })
        .await
        .unwrap();
    let stale = FinalVerificationSuccessEvidence {
        persisted_run_id: "stale-auto".into(),
        completed_at: "2026-01-01T00:00:00Z".into(),
        ordered_commands: serde_json::json!([]),
        covered_checks: serde_json::json!(["format", "slot-clippy"]),
        required_checks: material.required_checks.clone(),
        verification_input_fingerprint: fingerprint.clone(),
        manifest_version: "manifest-v1".into(),
        environment_identity_digest: persisted.digest,
    };
    let mut failed = fallback_evidence(&material, fingerprint, current);
    failed.manifest_version = manifest_version;
    failed.commands[0].exit_code = Some(1);
    failed.eligibility_reason = Some(FinalVerificationIneligibilityReason::CommandFailed {
        check_id: failed.commands[0].descriptor.check_id.clone(),
        exit_code: Some(1),
    });
    let callbacks = Arc::new(CompletionIntentCallbacks::for_reuse_with_evidence(
        task.id.clone(),
        material,
        Some(failed),
        false,
        None,
    ));
    let ctx = test_helpers::agent_context_from_db_with_callbacks(db.clone(), callbacks.clone());
    let intent = CompletionIntent {
        finalize_payload: serde_json::json!({"task_id":task.id,"commit_title":"auto","summary":"auto","files_changed":[],"remaining_concerns":[]}),
        tool_use_id: "auto-stale-identity".into(),
        final_verification_evidence: Some(stale),
    };
    assert!(!process_auto_submit_payload(&intent, &task.id, &ctx).await);
    assert!(callbacks.reuse_events().contains(&"canonical-execution"));
    let activity = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone())
        .list_activity(&task.id)
        .await
        .unwrap();
    assert!(
        activity
            .iter()
            .all(|entry| entry.event_type != "work_submitted")
    );
    assert_eq!(
        TaskAttemptRepository::new(db)
            .get(&attempt_id)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "pending"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_null_workspace_blocks_direct_and_auto_submit_without_consuming_evidence() {
    for (name, auto_submit) in [("direct", false), ("auto", true)] {
        let worktree = init_git_repo_with_dirty_file();
        let material = reuse_material_with_fingerprint_config(
            worktree.path().to_path_buf(),
            VerificationInputFingerprintConfig::default(),
        );
        assert!(
            !material.required_checks.is_empty(),
            "{name}: this regression must exercise a configured plan"
        );
        let fingerprint = c2_fingerprint(
            worktree.path(),
            &material.execution_request.fingerprint_config,
        )
        .await;
        let identity = djinn_core::canonical_verify::EnvironmentIdentityV1::derive(
            (material.execution_request.resolve_environment_identity)().unwrap(),
        )
        .unwrap();
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;
        assert_eq!(
            TaskRunRepository::new(db.clone())
                .get(&run_id)
                .await
                .unwrap()
                .unwrap()
                .workspace_path,
            None,
            "{name}: k8s dispatch row must remain NULL before configured resolution"
        );
        let attempt_id = uuid::Uuid::now_v7().to_string();
        TaskAttemptRepository::new(db.clone())
            .create_or_get_pending(CreateTaskAttemptParams {
                id: &attempt_id,
                task_id: &task.id,
                role: "worker",
                dispatch_key: &format!("null-workspace-{name}"),
                session_id: None,
                attempt_seq: None,
            })
            .await
            .unwrap();
        let stale = FinalVerificationSuccessEvidence {
            persisted_run_id: format!("stale-null-workspace-{name}"),
            completed_at: "2026-01-01T00:00:00Z".into(),
            ordered_commands: serde_json::json!([{"descriptor_id":"format"},{"descriptor_id":"slot-clippy"}]),
            covered_checks: serde_json::json!(["format", "slot-clippy"]),
            required_checks: material.required_checks.clone(),
            verification_input_fingerprint: fingerprint,
            manifest_version: "manifest-v1".into(),
            environment_identity_digest: identity.digest,
        };
        let callbacks = Arc::new(CompletionIntentCallbacks::for_reuse_requiring_workspace(
            task.id.clone(),
            material,
        ));
        let ctx = test_helpers::agent_context_from_db_with_callbacks(db.clone(), callbacks.clone());
        let intent = CompletionIntent {
            finalize_payload: serde_json::json!({"task_id":task.id,"commit_title":"NULL workspace","summary":"must fail closed","files_changed":[],"remaining_concerns":[]}),
            tool_use_id: format!("null-workspace-{name}"),
            final_verification_evidence: Some(stale),
        };
        let accepted = if auto_submit {
            process_auto_submit_payload(&intent, &task.id, &ctx).await
        } else {
            process_completion_intent_with_outcome(&intent, "submit_work", &task.id, &ctx).await
        };
        assert!(
            !accepted,
            "{name}: configured NULL workspace must fail closed"
        );
        assert!(
            callbacks.reuse_events().contains(&"writer-resolution"),
            "{name}: production completion validation must reach configured resolution"
        );
        assert!(
            !callbacks.reuse_events().contains(&"canonical-execution"),
            "{name}: no workspace means no replacement can consume stale evidence"
        );
        let activity = djinn_db::TaskRepository::new(db.clone(), ctx.event_bus.clone())
            .list_activity(&task.id)
            .await
            .unwrap();
        assert!(
            activity
                .iter()
                .all(|entry| entry.event_type != "work_submitted"),
            "{name}: failed C2 emits no successful submission"
        );
        assert_eq!(
            TaskAttemptRepository::new(db)
                .get(&attempt_id)
                .await
                .unwrap()
                .unwrap()
                .outcome,
            "pending",
            "{name}: failed C2 leaves the attempt pending"
        );
    }
}
