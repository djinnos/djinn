//! Repository-level ledger replay, interleaving, and rollback contracts.

use djinn_core::events::EventBus;
use djinn_core::models::{ReworkDelivery, TaskDeliveryIdentity, TaskDeliveryState, TaskIntegrated};
use djinn_db::{
    Database, DeliveryFinalizeInput, DeliveryPrepareInput, DeliveryReworkInput,
    DeliveryTransitionResult, TaskIntegrationResult, TaskRepository,
};

async fn fixture() -> (Database, TaskRepository) {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    for sql in [
        "UPDATE direct_delivery_epochs SET state = 'active', generation = 1",
        "INSERT INTO users (id, github_id, github_login) VALUES ('u', 9000002101, 'u')",
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ('p', 'p', 'owner', 'repo')",
        "INSERT INTO proposals (id, short_id, title) VALUES ('proposal', 'proposal', 'proposal')",
        "INSERT INTO proposal_build_attempts (id, proposal_id, short_id, lifecycle, base_sha, branch_name, branch_head_sha) VALUES ('attempt', 'proposal', 'attempt', 'active', 'parent', 'proposal/p/a', 'parent')",
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ('task', 'p', 'task', 'task', '', '', '[]', '[]', '[]', 'u')",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    (db.clone(), TaskRepository::new(db, EventBus::noop()))
}

fn id(generation: i64) -> TaskDeliveryIdentity {
    TaskDeliveryIdentity::new("attempt", "task", generation).unwrap()
}

fn prepare(generation: i64, source: &str, candidate: &str) -> DeliveryPrepareInput {
    DeliveryPrepareInput {
        identity: id(generation),
        transition_id: format!("prepare-{generation}"),
        source_sha: source.into(),
        patch_digest: format!("patch-{generation}"),
        selected_parent_sha: "parent".into(),
        candidate_sha: candidate.into(),
    }
}

async fn make_conflict(repo: &TaskRepository) {
    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    repo.begin_delivery_apply(&DeliveryFinalizeInput {
        identity: id(1),
        transition_id: "apply-1".into(),
        conflict_reason: None,
    })
    .await
    .unwrap();
    repo.finalize_delivery_conflict(&DeliveryFinalizeInput {
        identity: id(1),
        transition_id: "conflict-1".into(),
        conflict_reason: Some("conflict".into()),
    })
    .await
    .unwrap();
}

fn rework(transition: &str, candidate: &str) -> DeliveryReworkInput {
    DeliveryReworkInput {
        rework: ReworkDelivery::new(transition, "attempt", "task", 1, 2).unwrap(),
        source_sha: "source-2".into(),
        patch_digest: "patch-2".into(),
        selected_parent_sha: "parent".into(),
        candidate_sha: candidate.into(),
    }
}

#[tokio::test]
async fn conflict_requires_applying_and_rework_replay_and_races_are_serialized() {
    let (_db, repo) = fixture().await;
    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    assert!(matches!(
        repo.finalize_delivery_conflict(&DeliveryFinalizeInput {
            identity: id(1),
            transition_id: "early".into(),
            conflict_reason: Some("x".into()),
        })
        .await
        .unwrap(),
        DeliveryTransitionResult::Stale { .. }
    ));
    repo.begin_delivery_apply(&DeliveryFinalizeInput {
        identity: id(1),
        transition_id: "apply-1".into(),
        conflict_reason: None,
    })
    .await
    .unwrap();
    repo.finalize_delivery_conflict(&DeliveryFinalizeInput {
        identity: id(1),
        transition_id: "conflict-1".into(),
        conflict_reason: Some("x".into()),
    })
    .await
    .unwrap();
    assert!(matches!(
        repo.rework_delivery(&rework("rework", "candidate-2"))
            .await
            .unwrap(),
        DeliveryTransitionResult::Applied(_)
    ));
    assert!(matches!(
        repo.rework_delivery(&rework("rework", "candidate-2"))
            .await
            .unwrap(),
        DeliveryTransitionResult::Replayed(_)
    ));
    assert!(
        repo.rework_delivery(&rework("rework", "different"))
            .await
            .is_err()
    );

    let (_db, repo) = fixture().await;
    make_conflict(&repo).await;
    let left_input = rework("race-left", "candidate-left");
    let right_input = rework("race-right", "candidate-right");
    let (left, right) = tokio::join!(
        repo.rework_delivery(&left_input),
        repo.rework_delivery(&right_input)
    );
    assert_eq!(
        [left.unwrap(), right.unwrap()]
            .iter()
            .filter(|result| matches!(result, DeliveryTransitionResult::Applied(_)))
            .count(),
        1
    );
    assert_eq!(
        repo.get_delivery(&id(1)).await.unwrap().unwrap().state,
        TaskDeliveryState::Conflict
    );
}

#[tokio::test]
async fn integration_head_cas_late_failure_rollback_and_exact_replay() {
    let (db, repo) = fixture().await;
    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    repo.begin_delivery_apply(&DeliveryFinalizeInput {
        identity: id(1),
        transition_id: "apply-1".into(),
        conflict_reason: None,
    })
    .await
    .unwrap();
    sqlx::query("UPDATE tasks SET status = 'approved' WHERE id = 'task'")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE proposal_build_attempts SET branch_head_sha = 'wrong' WHERE id = 'attempt'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let input = TaskIntegrated::new(id(1), "candidate-1", "candidate-1", "candidate-1").unwrap();
    assert!(matches!(
        repo.task_integrated(&input).await.unwrap(),
        TaskIntegrationResult::Stale { .. }
    ));
    assert_eq!(
        repo.get_delivery(&id(1)).await.unwrap().unwrap().state,
        TaskDeliveryState::Applying
    );

    sqlx::query(
        "UPDATE proposal_build_attempts SET branch_head_sha = 'parent' WHERE id = 'attempt'",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("CREATE FUNCTION fail_delivery_activity() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'late failure'; END $$")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_delivery_activity BEFORE INSERT ON activity_log FOR EACH ROW EXECUTE FUNCTION fail_delivery_activity()")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(repo.task_integrated(&input).await.is_err());
    assert_eq!(
        repo.get_delivery(&id(1)).await.unwrap().unwrap().state,
        TaskDeliveryState::Applying
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT branch_head_sha FROM proposal_build_attempts WHERE id = 'attempt'"
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        "parent"
    );
    sqlx::query("DROP TRIGGER fail_delivery_activity ON activity_log")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION fail_delivery_activity()")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(matches!(
        repo.task_integrated(&input).await.unwrap(),
        TaskIntegrationResult::Integrated(_)
    ));
    assert!(matches!(
        repo.task_integrated(&input).await.unwrap(),
        TaskIntegrationResult::Replayed(_)
    ));
    let fabricated =
        TaskIntegrated::new(id(2), "candidate-1", "candidate-1", "candidate-1").unwrap();
    assert!(matches!(
        repo.task_integrated(&fabricated).await.unwrap(),
        TaskIntegrationResult::Stale { delivery: None, .. }
    ));
}
