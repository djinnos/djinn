//! Contract coverage for immutable command provenance and server-derived health.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::tools::evidence_command::{
    EvidenceCommandError, EvidenceCommandHealth, EvidenceCommandInvocationSelection,
    ServerCommandObservation, hydrate_command_provenance, hydrate_selected_command_provenance,
    record_command_observation,
};
use djinn_control_plane::tools::evidence_plan::{
    EvidenceMethod, EvidencePlanCapture, EvidencePlanCheckInput, EvidencePlanIdentity,
    capture_evidence_plan,
};
use djinn_db::{Database, EvidenceRepository};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    observations: Vec<ObservationCase>,
}
#[derive(Deserialize)]
struct ObservationCase {
    name: String,
    exit_code: Option<i32>,
    signal: Option<i32>,
    runner_failure: Option<String>,
    #[serde(default)]
    timed_out: bool,
    expected_health: EvidenceCommandHealth,
}
fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/evidence_command_invocations.json"))
        .expect("fixture valid")
}

async fn identity(db: &Database) -> EvidencePlanIdentity {
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let session = common::create_test_session(db, &project.id, &task.id).await;
    EvidencePlanIdentity {
        spike_task_id: task.id,
        session_id: session.id,
        captured_commit_sha: "commit".into(),
        worktree_fingerprint: "worktree".into(),
    }
}
fn observation(case: &ObservationCase) -> ServerCommandObservation {
    ServerCommandObservation {
        argv: vec!["cargo".into(), "test".into()],
        canonical_cwd: "/workspace/repo".into(),
        launch_state: "launched".into(),
        process_state: if case.timed_out {
            "timed_out"
        } else {
            "exited"
        }
        .into(),
        launched_at: Some("start".into()),
        finished_at: Some("end".into()),
        exit_code: case.exit_code,
        signal: case.signal,
        runner_failure: case.runner_failure.clone(),
        elapsed_millis: Some(10),
        timeout_millis: Some(100),
        timed_out: case.timed_out,
        stdout_digest: Some("stdout-digest".into()),
        stdout_excerpt: Some("stdout excerpt".into()),
        stdout_truncated: true,
        stderr_digest: Some("stderr-digest".into()),
        stderr_excerpt: Some("stderr excerpt".into()),
        stderr_truncated: false,
    }
}

#[tokio::test]
async fn evidence_command_provenance_contract() {
    let db = Database::open_in_memory().expect("database");
    let repository = EvidenceRepository::new(db.clone());
    let identity = identity(&db).await;
    capture_evidence_plan(
        &repository,
        identity.clone(),
        EvidencePlanCapture {
            checks: vec![
                EvidencePlanCheckInput {
                    check_id: "command_a".into(),
                    question: "Run A?".into(),
                    method: EvidenceMethod::Command,
                },
                EvidencePlanCheckInput {
                    check_id: "command_b".into(),
                    question: "Run B?".into(),
                    method: EvidenceMethod::Command,
                },
                EvidencePlanCheckInput {
                    check_id: "code".into(),
                    question: "Read it?".into(),
                    method: EvidenceMethod::Code,
                },
            ],
        },
    )
    .await
    .expect("capture");
    assert!(
        serde_json::from_str::<EvidenceCommandInvocationSelection>(
            r#"{"invocation_id":"id","exit_code":0}"#
        )
        .is_err()
    );
    assert!(matches!(
        record_command_observation(
            &repository,
            &identity,
            "code",
            observation(&fixture().observations[0])
        )
        .await,
        Err(EvidenceCommandError::MethodMismatch { .. })
    ));
    assert!(matches!(
        record_command_observation(
            &repository,
            &identity,
            "invented",
            observation(&fixture().observations[0])
        )
        .await,
        Err(EvidenceCommandError::UnknownCheck { .. })
    ));
    let mut other_commit = identity.clone();
    other_commit.captured_commit_sha = "other".into();
    assert!(matches!(
        record_command_observation(
            &repository,
            &other_commit,
            "command",
            observation(&fixture().observations[0])
        )
        .await,
        Err(EvidenceCommandError::Plan(_))
    ));
    let mut other_task = identity.clone();
    other_task.spike_task_id = "other-task".into();
    assert!(matches!(
        record_command_observation(
            &repository,
            &other_task,
            "command",
            observation(&fixture().observations[0])
        )
        .await,
        Err(EvidenceCommandError::Plan(_))
    ));
    let mut other_session = identity.clone();
    other_session.session_id = "other-session".into();
    assert!(matches!(
        record_command_observation(
            &repository,
            &other_session,
            "command",
            observation(&fixture().observations[0])
        )
        .await,
        Err(EvidenceCommandError::Plan(_))
    ));
    let mut other_worktree = identity.clone();
    other_worktree.worktree_fingerprint = "other-worktree".into();
    assert!(matches!(
        record_command_observation(
            &repository,
            &other_worktree,
            "command",
            observation(&fixture().observations[0])
        )
        .await,
        Err(EvidenceCommandError::Plan(_))
    ));
    let cases = fixture().observations;
    let first =
        record_command_observation(&repository, &identity, "command_a", observation(&cases[0]))
            .await
            .expect("first");
    let retry =
        record_command_observation(&repository, &identity, "command_a", observation(&cases[0]))
            .await
            .expect("retry");
    assert_ne!(first.id, retry.id);
    let other_check =
        record_command_observation(&repository, &identity, "command_b", observation(&cases[0]))
            .await
            .expect("other command check");
    assert!(matches!(
        hydrate_selected_command_provenance(
            &repository,
            &identity,
            "command_a",
            &EvidenceCommandInvocationSelection {
                invocation_id: "invented".into()
            }
        )
        .await,
        Err(EvidenceCommandError::UnknownInvocation { .. })
    ));
    assert!(matches!(
        hydrate_selected_command_provenance(
            &repository,
            &identity,
            "command_a",
            &EvidenceCommandInvocationSelection {
                invocation_id: other_check.id
            }
        )
        .await,
        Err(EvidenceCommandError::InvocationCheckMismatch {
            expected_check_id,
            actual_check_id,
        }) if expected_check_id == "command_a" && actual_check_id == "command_b"
    ));
    for case in &cases[1..] {
        record_command_observation(&repository, &identity, "command_a", observation(case))
            .await
            .expect("append");
    }
    let hydrated = hydrate_command_provenance(&repository, &identity)
        .await
        .expect("hydrate");
    assert_eq!(hydrated.len(), cases.len() + 2);
    assert_eq!(
        hydrated[0].invocation.argv,
        vec!["cargo".to_owned(), "test".to_owned()]
    );
    assert_eq!(hydrated[0].invocation.canonical_cwd, "/workspace/repo");
    assert_eq!(
        hydrated[0].invocation.stdout_digest.as_deref(),
        Some("stdout-digest")
    );
    assert!(hydrated[0].invocation.stdout_truncated);
    assert_eq!(hydrated[0].health, EvidenceCommandHealth::Ok);
    assert_eq!(hydrated[1].health, EvidenceCommandHealth::Ok);
    for (event, case) in hydrated.iter().skip(3).zip(&cases[1..]) {
        assert_eq!(event.health, case.expected_health, "{}", case.name);
    }
}
