//! Contract coverage for frozen refinement-evidence plans.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::tools::evidence_plan::{
    EvidencePlanCapture, EvidencePlanError, EvidencePlanIdentity, EvidenceTerminalResult,
    capture_evidence_plan, reconcile_terminal_results, require_frozen_plan,
};
use djinn_db::{Database, EvidenceRepository};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    valid_capture: EvidencePlanCapture,
    invalid_captures: Vec<EvidencePlanCapture>,
    reconciliations: Vec<ReconciliationCase>,
}

#[derive(Deserialize)]
struct ReconciliationCase {
    name: String,
    results: Vec<EvidenceTerminalResult>,
    ok: bool,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/evidence_plan_cases.json"))
        .expect("evidence plan fixture must be valid")
}

async fn identity(db: &Database) -> EvidencePlanIdentity {
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let session = common::create_test_session(db, &project.id, &task.id).await;
    EvidencePlanIdentity {
        spike_task_id: task.id,
        session_id: session.id,
        captured_commit_sha: "captured-commit-sha".to_owned(),
        worktree_fingerprint: "captured-worktree-fingerprint".to_owned(),
    }
}

#[tokio::test]
async fn evidence_plan_contract() {
    let fixture = fixture();
    let db = Database::open_in_memory().expect("open database");
    let repository = EvidenceRepository::new(db.clone());
    let identity = identity(&db).await;

    assert!(
        serde_json::from_str::<EvidencePlanCapture>(
            r#"{"checks":[{"check_id":"one","question":"Question?","method":"other"}]}"#,
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<EvidencePlanCapture>(
            r#"{"checks":[],"session_id":"caller-must-not-set-this"}"#,
        )
        .is_err()
    );

    // No plan can authorize execution or completion, including a different
    // session or changed provenance identity.
    assert_eq!(
        require_frozen_plan(&repository, &identity)
            .await
            .unwrap_err(),
        EvidencePlanError::NoFrozenPlan
    );

    for capture in fixture.invalid_captures {
        assert!(matches!(
            capture_evidence_plan(&repository, identity.clone(), capture).await,
            Err(EvidencePlanError::InvalidPlan(_))
        ));
    }

    let plan_id = capture_evidence_plan(&repository, identity.clone(), fixture.valid_capture)
        .await
        .expect("valid plan capture");
    let frozen = require_frozen_plan(&repository, &identity)
        .await
        .expect("matching plan exists");
    assert_eq!(frozen.plan.id, plan_id);
    assert_eq!(frozen.plan.spike_task_id, identity.spike_task_id);
    assert_eq!(frozen.plan.session_id, identity.session_id);
    assert_eq!(
        frozen.plan.captured_commit_sha,
        identity.captured_commit_sha
    );
    assert_eq!(
        frozen.plan.worktree_fingerprint,
        identity.worktree_fingerprint
    );
    assert!(frozen.finalized_projection.is_none());

    // Second capture is rejected by the durable one-shot identity constraint;
    // it cannot overwrite checks or provenance on the already-frozen plan.
    let second = capture_evidence_plan(&repository, identity.clone(), fixture_capture()).await;
    assert!(matches!(second, Err(EvidencePlanError::Persistence(_))));
    let still_frozen = require_frozen_plan(&repository, &identity).await.unwrap();
    assert_eq!(still_frozen.plan, frozen.plan);
    assert!(still_frozen.finalized_projection.is_none());

    let mut wrong_commit = identity.clone();
    wrong_commit.captured_commit_sha = "other-commit".to_owned();
    assert_eq!(
        require_frozen_plan(&repository, &wrong_commit)
            .await
            .unwrap_err(),
        EvidencePlanError::NoFrozenPlan
    );

    for case in fixture().reconciliations {
        let result = reconcile_terminal_results(&frozen.plan, &case.results);
        assert_eq!(result.is_ok(), case.ok, "case {}", case.name);
        // Rejection is pure and therefore cannot create a final hand-off or
        // mutate the frozen ordered plan.
        let after = require_frozen_plan(&repository, &identity).await.unwrap();
        assert_eq!(after.plan, frozen.plan, "case {}", case.name);
        assert!(after.finalized_projection.is_none(), "case {}", case.name);
    }
}

// Make a second capture request from the fixture's valid input without
// duplicating the JSON structure in the contract body.
fn fixture_capture() -> EvidencePlanCapture {
    fixture().valid_capture
}
