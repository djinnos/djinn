use super::*;
use djinn_core::{
    canonical_verify::{CanonicalCommandDescriptorV1, EnvironmentIdentityV1},
    models::VerifySource,
};
use djinn_db::repositories::{
    task_run::{CreateTaskRunParams, TaskRunRepository},
    verify_run::VerifyRunRepository,
};
use djinn_git::{VerificationInputDigestV1, VerificationInputFingerprintConfig};
use djinn_sandbox::final_verification_execution::{
    FinalVerificationCommandEvidence, FinalVerificationExecutionEvidence,
    FinalVerificationIneligibilityReason,
};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

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
#[tokio::test]
async fn canonical_multi_command_evidence_records_one_complete_row() {
    let (ctx, request) = fixture().await;
    let outcome = persist_evidence(
        &request,
        "attempt-one",
        "run-one",
        &material(vec!["lint".into(), "test".into()]),
        &passing("post-authoring-fingerprint"),
        &ctx,
    )
    .await;
    assert_eq!(
        outcome,
        FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id: "attempt-one".into(),
            verify_run_id: "run-one".into()
        }
    );
    let rows = VerifyRunRepository::new(ctx.db.clone())
        .list_for_task_run(&request.task_run_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.verification_attempt_id.as_deref(), Some("attempt-one"));
    assert_eq!(row.verify_run_id, "run-one");
    assert_eq!(row.source_phase.as_deref(), Some("final_verification"));
    assert_eq!(
        row.covered_checks,
        Some(serde_json::json!(["lint", "test"]))
    );
    let ordered: Vec<_> = row
        .ordered_commands
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["descriptor_id"].as_str().unwrap())
        .collect();
    assert_eq!(ordered, vec!["lint", "test"]);
    assert_eq!(
        row.verification_input_fingerprint.as_deref(),
        Some("post-authoring-fingerprint")
    );
    assert_eq!(row.manifest_version.as_deref(), Some("manifest-v1"));
    assert_eq!(
        row.environment_identity_digest.as_deref(),
        Some("identity-stable")
    );
}
#[tokio::test]
async fn consistency_check_gap_and_insert_failures_record_no_pass() {
    let (ctx, request) = fixture().await;
    let resolved = material(vec!["lint".into(), "test".into()]);
    let mut mutation = passing("f0");
    mutation.fingerprint_f1 = Some(fingerprint("f1"));
    let mut environment = passing("same");
    environment.post_environment_identity = Some(identity("changed"));
    for evidence in [mutation, environment] {
        assert!(matches!(
            persist_evidence(&request, "attempt", "run", &resolved, &evidence, &ctx).await,
            FinalVerificationRecordingOutcome::Ineligible { .. }
        ));
    }
    let gap = material(vec!["lint".into(), "test".into(), "docs".into()]);
    assert!(matches!(
        persist_evidence(
            &request,
            "attempt-gap",
            "run-gap",
            &gap,
            &passing("gap"),
            &ctx
        )
        .await,
        FinalVerificationRecordingOutcome::Error { .. }
    ));
    let bad = FinalVerificationCoordinatorRequest {
        task_run_id: "missing-run".into(),
        ..request.clone()
    };
    assert!(matches!(
        persist_evidence(
            &bad,
            "attempt-insert",
            "run-insert",
            &resolved,
            &passing("insert"),
            &ctx
        )
        .await,
        FinalVerificationRecordingOutcome::Error { .. }
    ));
    assert!(
        VerifyRunRepository::new(ctx.db.clone())
            .list_for_task_run(&request.task_run_id)
            .await
            .unwrap()
            .is_empty()
    );
}
#[test]
fn executor_failure_matrix_has_bounded_ineligible_outcomes() {
    use FinalVerificationIneligibilityReason as R;
    let reasons = vec![
        R::CommandFailed {
            check_id: "test".into(),
            exit_code: Some(1),
        },
        R::CommandTimedOut {
            check_id: "test".into(),
        },
        R::NonHermeticPlan,
        R::UndeclaredCommandEnvironment {
            name: "AD_HOC".into(),
        },
        R::SandboxViolation {
            detail: "violation".into(),
        },
        R::FingerprintChanged,
        R::EnvironmentChanged,
        R::RequiredChecksNotCovered {
            missing: vec!["test".into()],
        },
        R::FingerprintUnavailable {
            detail: "input".into(),
        },
        R::EnvironmentIdentityUnavailable {
            detail: "identity".into(),
        },
        R::ManifestBindingMismatch {
            detail: "manifest".into(),
        },
    ];
    for reason in reasons {
        let evidence = FinalVerificationExecutionEvidence {
            eligibility_reason: Some(reason),
            ..passing("never")
        };
        assert!(!evidence.eligible());
        assert_ne!(
            format_evidence_reason(&evidence),
            "malformed final-verification evidence"
        );
    }
}
#[tokio::test]
async fn setup_pass_is_excluded_and_only_post_authoring_fingerprint_is_stored() {
    let (ctx, request) = fixture().await;
    let tree = crate::test_helpers::test_path("setup-vs-final-");
    let file = tree.join("authored.txt");
    std::fs::write(&file, "setup state").unwrap();
    assert!(
        VerifyRunRepository::new(ctx.db.clone())
            .list_for_task_run(&request.task_run_id)
            .await
            .unwrap()
            .is_empty()
    );
    std::fs::write(&file, "post-authoring edit").unwrap();
    let post = std::fs::read_to_string(&file).unwrap();
    assert!(matches!(
        persist_evidence(
            &request,
            "final-attempt",
            "final-run",
            &material(vec!["lint".into(), "test".into()]),
            &passing(&format!("tree:{post}")),
            &ctx
        )
        .await,
        FinalVerificationRecordingOutcome::Stored { .. }
    ));
    let rows = VerifyRunRepository::new(ctx.db.clone())
        .list_for_task_run(&request.task_run_id)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].verification_input_fingerprint.as_deref(),
        Some("tree:post-authoring edit")
    );
}
