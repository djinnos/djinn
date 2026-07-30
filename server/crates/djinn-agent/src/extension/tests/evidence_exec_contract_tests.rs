//! Cross-crate contracts for the frozen-plan command execution seam.
//!
//! The request boundary is deliberately split: sandbox-policy failures happen
//! before `record_command_observation`, while every server observation is
//! appended to the immutable foundation ledger and re-hydrated through the
//! control-plane health mapper.

use super::*;
use djinn_control_plane::tools::evidence_command::{
    EvidenceCommandHealth, ServerCommandObservation, hydrate_command_provenance,
    record_command_observation,
};
use djinn_control_plane::tools::evidence_plan::{
    EvidenceMethod, EvidencePlanCapture, EvidencePlanCheckInput, EvidencePlanIdentity,
    capture_evidence_plan, require_frozen_plan,
};
use djinn_core::events::EventBus;
use djinn_db::{
    EffectiveCreatorProvenance, EpicCreateInput, EpicRepository, EvidenceRepository,
    ProjectRepository, SessionRepository, TaskRepository, UserRepository,
    repositories::session::CreateSessionParams,
};
use djinn_sandbox::{
    EVIDENCE_MAX_OUTPUT_BYTES, EVIDENCE_MAX_TIMEOUT, EvidenceRequest, EvidenceSandbox,
};
use std::time::Duration;

async fn fixture() -> (EvidenceRepository, EvidencePlanIdentity) {
    let db = create_test_db();
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create(
            &format!("evidence-contract-{suffix}"),
            "test",
            &format!("evidence-{suffix}"),
        )
        .await
        .expect("project");
    let epic = EpicRepository::new(db.clone(), EventBus::noop())
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "evidence contract",
                description: "test",
                emoji: "🧪",
                color: "blue",
                owner: "test",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("epic");
    let creator = UserRepository::new(db.clone())
        .upsert_from_github(
            9_999_000_000 + (uuid::Uuid::now_v7().as_u128() % 100_000) as i64,
            "evidence-contract",
            None,
            None,
        )
        .await
        .expect("creator");
    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
            "evidence contract",
            "test",
            "test",
            "task",
            1,
            "test",
            None,
            None,
        )
        .await
        .expect("task");
    let session = SessionRepository::new(db.clone(), EventBus::noop())
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("session");
    (
        EvidenceRepository::new(db),
        EvidencePlanIdentity {
            spike_task_id: task.id,
            session_id: session.id,
            captured_commit_sha: "server-commit".into(),
            worktree_fingerprint: "server-worktree".into(),
        },
    )
}

fn observation(
    exit_code: Option<i32>,
    timed_out: bool,
    signal: Option<i32>,
    runner_failure: Option<&str>,
) -> ServerCommandObservation {
    ServerCommandObservation {
        argv: vec!["cat".into(), "README.md".into()],
        canonical_cwd: "/server/canonical/worktree".into(),
        launch_state: if runner_failure.is_some() {
            "failed_to_launch"
        } else {
            "launched"
        }
        .into(),
        process_state: if timed_out {
            "timed_out"
        } else if runner_failure.is_some() {
            "runner_failed"
        } else if signal.is_some() {
            "signaled"
        } else {
            "exited"
        }
        .into(),
        launched_at: Some("server-start".into()),
        finished_at: Some("server-end".into()),
        exit_code,
        signal,
        runner_failure: runner_failure.map(str::to_owned),
        elapsed_millis: Some(17),
        timeout_millis: Some(100),
        timed_out,
        stdout_digest: Some("server-stdout-digest".into()),
        stdout_excerpt: Some("server stdout".into()),
        stdout_truncated: true,
        stderr_digest: Some("server-stderr-digest".into()),
        stderr_excerpt: Some("server stderr".into()),
        stderr_truncated: false,
    }
}

async fn capture(repository: &EvidenceRepository, identity: EvidencePlanIdentity) {
    capture_evidence_plan(
        repository,
        identity,
        EvidencePlanCapture {
            checks: vec![
                EvidencePlanCheckInput {
                    check_id: "command".into(),
                    question: "run read?".into(),
                    method: EvidenceMethod::Command,
                },
                EvidencePlanCheckInput {
                    check_id: "code".into(),
                    question: "read source?".into(),
                    method: EvidenceMethod::Code,
                },
            ],
        },
    )
    .await
    .expect("frozen plan");
}

#[tokio::test]
async fn evidence_exec_contract_rejects_preflight_without_ledger_events() {
    let (repository, identity) = fixture().await;
    assert!(
        require_frozen_plan(&repository, &identity).await.is_err(),
        "a missing frozen plan must stop execution"
    );
    assert!(
        hydrate_command_provenance(&repository, &identity)
            .await
            .is_err()
    );

    capture(&repository, identity.clone()).await;
    for (check_id, expected) in [("unknown", "unknown"), ("code", "requires method")] {
        let error = record_command_observation(
            &repository,
            &identity,
            check_id,
            observation(Some(0), false, None, None),
        )
        .await;
        assert!(
            error.is_err(),
            "{check_id} is rejected before an observation is appended"
        );
        assert!(error.unwrap_err().to_string().contains(expected));
    }
    let mut cross_identity = identity.clone();
    cross_identity.captured_commit_sha = "other-commit".into();
    assert!(
        record_command_observation(
            &repository,
            &cross_identity,
            "command",
            observation(Some(0), false, None, None)
        )
        .await
        .is_err()
    );

    // The closed sandbox validates malformed argv, escaped cwd, and invalid
    // timeouts before it reaches its `run_isolated` process-start boundary.
    let root = crate::test_helpers::test_tempdir("evidence-exec-preflight-");
    let sandbox = EvidenceSandbox::new(root.path().to_path_buf());
    for request in [
        EvidenceRequest {
            argv: vec!["sh".into()],
            cwd: None,
            timeout: Duration::from_millis(1),
            output_limit: EVIDENCE_MAX_OUTPUT_BYTES,
        },
        EvidenceRequest {
            argv: vec!["cat".into()],
            cwd: Some("../".into()),
            timeout: Duration::from_millis(1),
            output_limit: EVIDENCE_MAX_OUTPUT_BYTES,
        },
        EvidenceRequest {
            argv: vec!["cat".into()],
            cwd: None,
            timeout: EVIDENCE_MAX_TIMEOUT + Duration::from_millis(1),
            output_limit: EVIDENCE_MAX_OUTPUT_BYTES,
        },
    ] {
        assert!(sandbox.run(request).await.is_err());
    }
    assert!(
        hydrate_command_provenance(&repository, &identity)
            .await
            .unwrap()
            .is_empty(),
        "all rejected requests leave no invocation event"
    );
}

#[tokio::test]
async fn evidence_exec_contract_preserves_server_observations_and_health_precedence() {
    let (repository, identity) = fixture().await;
    capture(&repository, identity.clone()).await;
    let cases = [
        (Some(0), false, None, None, EvidenceCommandHealth::Ok),
        (Some(7), false, None, None, EvidenceCommandHealth::Degraded),
        (
            Some(0),
            true,
            Some(9),
            Some("runner also failed"),
            EvidenceCommandHealth::Timeout,
        ),
        (
            None,
            false,
            None,
            Some("launch failed"),
            EvidenceCommandHealth::Error,
        ),
        (None, false, Some(9), None, EvidenceCommandHealth::Broken),
        (None, false, None, None, EvidenceCommandHealth::Broken),
    ];
    let first = record_command_observation(
        &repository,
        &identity,
        "command",
        observation(cases[0].0, cases[0].1, cases[0].2, cases[0].3),
    )
    .await
    .unwrap();
    let retry = record_command_observation(
        &repository,
        &identity,
        "command",
        observation(cases[0].0, cases[0].1, cases[0].2, cases[0].3),
    )
    .await
    .unwrap();
    assert_ne!(
        first.id, retry.id,
        "retries allocate a new immutable invocation id"
    );
    for case in cases.iter().skip(1) {
        record_command_observation(
            &repository,
            &identity,
            "command",
            observation(case.0, case.1, case.2, case.3),
        )
        .await
        .unwrap();
    }
    let hydrated = hydrate_command_provenance(&repository, &identity)
        .await
        .unwrap();
    assert_eq!(
        hydrated.len(),
        cases.len() + 1,
        "no prior invocation is overwritten"
    );
    for (event, case) in hydrated.iter().skip(2).zip(cases.iter().skip(1)) {
        assert_eq!(event.health, case.4);
    }
    let event = &hydrated[0].invocation;
    assert_eq!(event.argv, vec!["cat", "README.md"]);
    assert_eq!(event.canonical_cwd, "/server/canonical/worktree");
    assert_eq!(event.stdout_digest.as_deref(), Some("server-stdout-digest"));
    assert!(event.stdout_truncated);
    assert!(!event.stderr_truncated);
    assert_eq!(event.stdout_excerpt.as_deref(), Some("server stdout"));
}
