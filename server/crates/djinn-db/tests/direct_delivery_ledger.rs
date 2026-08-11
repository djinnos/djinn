//! Repository-level ledger replay, interleaving, and rollback contracts.

use djinn_core::events::EventBus;
use djinn_core::models::{
    MappedHeadRetryDelivery, ReworkDelivery, TaskDeliveryIdentity, TaskDeliveryState,
    TaskIntegrated, TransitionAction,
};
use djinn_db::{
    ChildDisposition, Database, DeliveryFinalizeInput, DeliveryMappedHeadRetryInput,
    DeliveryPrepareInput, DeliveryReworkInput, DeliveryTransitionResult, DispositionScope,
    TaskIntegrationResult, TaskRepository,
};
use std::sync::{Arc, Mutex};

async fn fixture() -> (Database, TaskRepository) {
    fixture_with_bus(EventBus::noop()).await
}

async fn fixture_with_bus(events: EventBus) -> (Database, TaskRepository) {
    let db = Database::ephemeral().await.unwrap();
    db.ensure_initialized().await.unwrap();
    for sql in [
        "UPDATE direct_delivery_epochs SET state = 'active', generation = 1",
        "INSERT INTO users (id, github_id, github_login) VALUES ('u', 9000002101, 'u')",
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ('p', 'p', 'owner', 'repo')",
        "INSERT INTO proposals (id, short_id, title) VALUES ('proposal', 'proposal', 'proposal')",
        "INSERT INTO epics (id, project_id, short_id, title, description, memory_refs, created_by_user_id, proposal_id) VALUES ('epic', 'p', 'epic', 'epic', '', '[]', 'u', 'proposal')",
        "INSERT INTO proposal_build_attempts (id, proposal_id, short_id, lifecycle, base_sha, branch_name, branch_head_sha) VALUES ('attempt', 'proposal', 'attempt', 'active', 'parent', 'proposal/p/a', 'parent')",
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ('task', 'p', 'task', 'epic', 'task', '', '', '[]', '[]', '[]', 'u')",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    (db.clone(), TaskRepository::new(db, events))
}

async fn assert_parent_and_blocker_wait(
    db: &Database,
    repo: &TaskRepository,
    expected_delivery_state: TaskDeliveryState,
    generation: i64,
) {
    assert_eq!(
        repo.get_delivery(&id(generation))
            .await
            .unwrap()
            .unwrap()
            .state,
        expected_delivery_state
    );
    assert!(
        repo.transition(
            "dependent",
            TransitionAction::Start,
            "",
            "system",
            None,
            None,
        )
        .await
        .is_err(),
        "a non-applied direct generation must not release its dependent"
    );
    let plan = repo
        .classify_parent_disposition(&DispositionScope::for_proposal_abort(
            "proposal",
            vec!["epic".into()],
        ))
        .await
        .unwrap();
    let task = plan.findings.iter().find(|f| f.task_id == "task").unwrap();
    assert_eq!(task.disposition, ChildDisposition::Park);
    assert_eq!(task.status, "approved");
    assert_ne!(
        sqlx::query_scalar::<_, String>("SELECT status FROM epics WHERE id = 'epic'")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        "closed",
        "delivery evidence alone must not complete the epic"
    );
}

fn prepare_for(
    task_id: &str,
    generation: i64,
    source: &str,
    candidate: &str,
) -> DeliveryPrepareInput {
    DeliveryPrepareInput {
        identity: id_for(task_id, generation),
        transition_id: format!("prepare-{task_id}-{generation}"),
        source_sha: source.into(),
        patch_digest: format!("patch-{task_id}-{generation}"),
        selected_parent_sha: "parent".into(),
        candidate_sha: candidate.into(),
    }
}

async fn begin_applying(repo: &TaskRepository, identity: TaskDeliveryIdentity) {
    repo.begin_delivery_apply(&DeliveryFinalizeInput {
        identity,
        transition_id: "apply".into(),
        conflict_reason: None,
    })
    .await
    .unwrap();
}

fn id(generation: i64) -> TaskDeliveryIdentity {
    TaskDeliveryIdentity::new("attempt", "task", generation).unwrap()
}

fn id_for(task_id: &str, generation: i64) -> TaskDeliveryIdentity {
    TaskDeliveryIdentity::new("attempt", task_id, generation).unwrap()
}

async fn seed_task(db: &Database, task_id: &str) {
    sqlx::query("INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) VALUES ($1, 'p', $1, $1, '', '', '[]', '[\"criterion\"]', '[]', 'u')")
        .bind(task_id)
        .execute(db.pool())
        .await
        .unwrap();
}

async fn approve(db: &Database, task_id: &str) {
    sqlx::query("UPDATE tasks SET status = 'approved' WHERE id = $1")
        .bind(task_id)
        .execute(db.pool())
        .await
        .unwrap();
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
        repo.finalize_delivery_conflict(&DeliveryFinalizeInput {
            identity: id(1),
            transition_id: "conflict-1".into(),
            conflict_reason: Some("x".into()),
        })
        .await
        .unwrap(),
        DeliveryTransitionResult::Replayed(_)
    ));
    assert!(matches!(
        repo.finalize_delivery_conflict(&DeliveryFinalizeInput {
            identity: id(1),
            transition_id: "conflict-1".into(),
            conflict_reason: Some("different conflict".into()),
        })
        .await
        .unwrap(),
        DeliveryTransitionResult::Stale { .. }
    ));
    let conflict = repo.get_delivery(&id(1)).await.unwrap().unwrap();
    assert_eq!(
        (conflict.state, conflict.conflict_reason.as_deref()),
        (TaskDeliveryState::Conflict, Some("x"))
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_parent_integrations_advance_only_one_attempt_head() {
    let (db, repo) = fixture().await;
    seed_task(&db, "task-a").await;
    seed_task(&db, "task-b").await;
    for (task, source, candidate) in [
        ("task-a", "source-a", "candidate-a"),
        ("task-b", "source-b", "candidate-b"),
    ] {
        repo.prepare_delivery(&prepare_for(task, 1, source, candidate))
            .await
            .unwrap();
        begin_applying(&repo, id_for(task, 1)).await;
        approve(&db, task).await;
    }
    let left = TaskIntegrated::new(
        id_for("task-a", 1),
        "candidate-a",
        "candidate-a",
        "candidate-a",
    )
    .unwrap();
    let right = TaskIntegrated::new(
        id_for("task-b", 1),
        "candidate-b",
        "candidate-b",
        "candidate-b",
    )
    .unwrap();
    let (left, right) = tokio::join!(repo.task_integrated(&left), repo.task_integrated(&right));
    let results = [left.unwrap(), right.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, TaskIntegrationResult::Integrated(_)))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, TaskIntegrationResult::Stale { .. }))
            .count(),
        1
    );
    let a_won = matches!(&results[0], TaskIntegrationResult::Integrated(_));
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT branch_head_sha FROM proposal_build_attempts WHERE id = 'attempt'"
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        if a_won { "candidate-a" } else { "candidate-b" }
    );
    for (task, candidate, won) in [
        ("task-a", "candidate-a", a_won),
        ("task-b", "candidate-b", !a_won),
    ] {
        assert_eq!(
            repo.get_delivery(&id_for(task, 1))
                .await
                .unwrap()
                .unwrap()
                .state,
            if won {
                TaskDeliveryState::Applied
            } else {
                TaskDeliveryState::Applying
            }
        );
        let task = repo.get(task).await.unwrap().unwrap();
        assert_eq!(task.status, if won { "closed" } else { "approved" });
        assert_eq!(task.merge_commit_sha.as_deref(), won.then_some(candidate));
    }
}

#[tokio::test]
async fn conflict_generation_is_not_integrable_and_remains_immutable() {
    let (db, repo) = fixture().await;
    make_conflict(&repo).await;
    approve(&db, "task").await;
    let conflict = TaskIntegrated::new(id(1), "candidate-1", "candidate-1", "candidate-1").unwrap();
    assert!(matches!(
        repo.task_integrated(&conflict).await.unwrap(),
        TaskIntegrationResult::Stale { .. }
    ));
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT branch_head_sha FROM proposal_build_attempts WHERE id = 'attempt'"
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        "parent"
    );
    let task = repo.get("task").await.unwrap().unwrap();
    assert_eq!(
        (task.status.as_str(), task.merge_commit_sha),
        ("approved", None)
    );
    let row = repo.get_delivery(&id(1)).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.candidate_sha.as_str()),
        (TaskDeliveryState::Conflict, "candidate-1")
    );
}

#[tokio::test]
async fn dependent_releases_only_after_corrected_generation_integrates() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let (db, repo) = fixture_with_bus(EventBus::new(move |event| {
        sink.lock().unwrap().push(event);
    }))
    .await;
    seed_task(&db, "dependent").await;
    sqlx::query("UPDATE tasks SET epic_id = 'epic' WHERE id = 'dependent'")
        .execute(db.pool())
        .await
        .unwrap();
    repo.add_blocker("dependent", "task").await.unwrap();
    approve(&db, "task").await;

    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    assert_parent_and_blocker_wait(&db, &repo, TaskDeliveryState::Prepared, 1).await;
    begin_applying(&repo, id(1)).await;
    assert_parent_and_blocker_wait(&db, &repo, TaskDeliveryState::Applying, 1).await;
    repo.finalize_delivery_conflict(&DeliveryFinalizeInput {
        identity: id(1),
        transition_id: "conflict-1".into(),
        conflict_reason: Some("conflict".into()),
    })
    .await
    .unwrap();
    assert_parent_and_blocker_wait(&db, &repo, TaskDeliveryState::Conflict, 1).await;

    let conflict = TaskIntegrated::new(id(1), "candidate-1", "candidate-1", "candidate-1").unwrap();
    assert!(matches!(
        repo.task_integrated(&conflict).await.unwrap(),
        TaskIntegrationResult::Stale { .. }
    ));
    let corrected = DeliveryReworkInput {
        rework: ReworkDelivery::new("corrected", "attempt", "task", 1, 2).unwrap(),
        source_sha: "source-2".into(),
        patch_digest: "patch-2".into(),
        selected_parent_sha: "parent".into(),
        candidate_sha: "candidate-2".into(),
    };
    assert!(matches!(
        repo.rework_delivery(&corrected).await.unwrap(),
        DeliveryTransitionResult::Applied(_)
    ));
    assert_parent_and_blocker_wait(&db, &repo, TaskDeliveryState::Prepared, 2).await;
    begin_applying(&repo, id(2)).await;
    assert_parent_and_blocker_wait(&db, &repo, TaskDeliveryState::Applying, 2).await;

    captured.lock().unwrap().clear();
    let integrated =
        TaskIntegrated::new(id(2), "candidate-2", "candidate-2", "candidate-2").unwrap();
    assert!(matches!(
        repo.task_integrated(&integrated).await.unwrap(),
        TaskIntegrationResult::Integrated(_)
    ));
    let blocker = repo.get("task").await.unwrap().unwrap();
    assert_eq!(
        (blocker.status.as_str(), blocker.merge_commit_sha.as_deref()),
        ("closed", Some("candidate-2"))
    );
    assert_eq!(
        repo.get_delivery(&id(1)).await.unwrap().unwrap().state,
        TaskDeliveryState::Conflict
    );
    assert_eq!(
        repo.get_delivery(&id(2)).await.unwrap().unwrap().state,
        TaskDeliveryState::Applied
    );
    let parent_plan = repo
        .classify_parent_disposition(&DispositionScope::for_proposal_abort(
            "proposal",
            vec!["epic".into()],
        ))
        .await
        .unwrap();
    assert_eq!(
        parent_plan
            .findings
            .iter()
            .find(|finding| finding.task_id == "task")
            .unwrap()
            .disposition,
        ChildDisposition::RetainedAlreadyTerminal
    );
    let dependent_updates = || {
        captured
            .lock()
            .unwrap()
            .iter()
            .filter(|event| {
                event.entity_type == "task"
                    && event.action == "updated"
                    && event.payload["task"]["id"] == "dependent"
            })
            .count()
    };
    assert_eq!(dependent_updates(), 1, "dependent releases exactly once");
    assert!(matches!(
        repo.task_integrated(&integrated).await.unwrap(),
        TaskIntegrationResult::Replayed(_)
    ));
    assert_eq!(dependent_updates(), 1, "replay must not release twice");
    assert_eq!(
        repo.transition(
            "dependent",
            TransitionAction::Start,
            "",
            "system",
            None,
            None
        )
        .await
        .unwrap()
        .status,
        "in_progress"
    );
}

#[tokio::test]
async fn disabled_and_explicit_legacy_keep_parent_and_blocker_semantics() {
    for (case, disable_epoch, explicit_pr) in [
        ("supported-disabled", true, false),
        ("explicit-legacy", false, true),
    ] {
        let (db, repo) = fixture().await;
        seed_task(&db, "dependent").await;
        sqlx::query("UPDATE tasks SET epic_id = 'epic' WHERE id = 'dependent'")
            .execute(db.pool())
            .await
            .unwrap();
        repo.add_blocker("dependent", "task").await.unwrap();
        repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
            .await
            .unwrap();
        if disable_epoch {
            sqlx::query("UPDATE direct_delivery_epochs SET state = 'disabled'")
                .execute(db.pool())
                .await
                .unwrap();
        }
        sqlx::query("UPDATE tasks SET status = 'closed', close_reason = 'completed', merge_commit_sha = 'legacy-sha', pr_url = $1 WHERE id = 'task'")
            .bind(explicit_pr.then_some("https://github.com/owner/repo/pull/1"))
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(
            repo.transition(
                "dependent",
                TransitionAction::Start,
                "",
                "system",
                None,
                None,
            )
            .await
            .unwrap()
            .status,
            "in_progress",
            "{case} must retain the legacy closed-blocker predicate"
        );
        let plan = repo
            .classify_parent_disposition(&DispositionScope::for_proposal_abort(
                "proposal",
                vec!["epic".into()],
            ))
            .await
            .unwrap();
        assert_eq!(
            plan.findings
                .iter()
                .find(|finding| finding.task_id == "task")
                .unwrap()
                .disposition,
            ChildDisposition::RetainedAlreadyTerminal,
            "{case} must retain legacy parent disposition"
        );
    }
}

/// A mapped head is a candidate already represented in the attempt ledger.
async fn seed_mapped_parent(db: &Database, repo: &TaskRepository) {
    seed_task(db, "mapped-task").await;
    repo.prepare_delivery(&prepare_for(
        "mapped-task",
        1,
        "mapped-source",
        "mapped-parent",
    ))
    .await
    .unwrap();
}

fn mapped_head_retry(
    transition_id: &str,
    source_sha: &str,
    selected_parent_sha: &str,
    candidate_sha: &str,
) -> DeliveryMappedHeadRetryInput {
    DeliveryMappedHeadRetryInput {
        retry: MappedHeadRetryDelivery::new(transition_id, "attempt", "task", 1, 2).unwrap(),
        source_sha: source_sha.into(),
        patch_digest: "patch-1".into(),
        selected_parent_sha: selected_parent_sha.into(),
        candidate_sha: candidate_sha.into(),
    }
}

#[tokio::test]
async fn mapped_head_retry_supersedes_prepared_with_exact_replay_and_immutable_history() {
    let (db, repo) = fixture().await;
    seed_mapped_parent(&db, &repo).await;
    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    let retry = mapped_head_retry(
        "supersede-prepared",
        "source-1",
        "mapped-parent",
        "candidate-2",
    );
    assert!(matches!(
        repo.retry_delivery_from_mapped_head(&retry).await.unwrap(),
        DeliveryTransitionResult::Applied(_)
    ));
    assert!(matches!(
        repo.retry_delivery_from_mapped_head(&retry).await.unwrap(),
        DeliveryTransitionResult::Replayed(_)
    ));
    let old = repo.get_delivery(&id(1)).await.unwrap().unwrap();
    let new = repo.get_delivery(&id(2)).await.unwrap().unwrap();
    assert_eq!(
        (
            old.state,
            old.candidate_sha.as_str(),
            old.source_sha.as_str(),
            old.patch_digest.as_str(),
            old.supersede_transition_id.as_deref(),
        ),
        (
            TaskDeliveryState::Superseded,
            "candidate-1",
            "source-1",
            "patch-1",
            Some("supersede-prepared"),
        )
    );
    assert_eq!(
        (
            new.state,
            new.source_sha.as_str(),
            new.patch_digest.as_str(),
            new.selected_parent_sha.as_str(),
            new.candidate_sha.as_str(),
        ),
        (
            TaskDeliveryState::Prepared,
            "source-1",
            "patch-1",
            "mapped-parent",
            "candidate-2",
        )
    );
    let mut mismatched_replay = retry.clone();
    mismatched_replay.candidate_sha = "different-candidate".into();
    assert!(
        repo.retry_delivery_from_mapped_head(&mismatched_replay)
            .await
            .is_err()
    );
    assert_eq!(repo.get_delivery(&id(1)).await.unwrap().unwrap(), old);
    assert_eq!(repo.get_delivery(&id(2)).await.unwrap().unwrap(), new);

    // The partial unique index permits no second live generation.
    assert!(sqlx::query("INSERT INTO task_deliveries (build_attempt_id, task_id, delivery_generation, state, candidate_sha, base_sha, source_sha, patch_digest, selected_parent_sha, prepare_transition_id) VALUES ('attempt', 'task', 3, 'prepared', 'candidate-3', 'mapped-parent', 'source-1', 'patch-1', 'mapped-parent', 'unexpected-live')")
        .execute(db.pool())
        .await
        .is_err());
}

#[tokio::test]
async fn mapped_head_retry_supersedes_applying_generation() {
    let (db, repo) = fixture().await;
    seed_mapped_parent(&db, &repo).await;
    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    begin_applying(&repo, id(1)).await;
    assert!(matches!(
        repo.retry_delivery_from_mapped_head(&mapped_head_retry(
            "supersede-applying",
            "source-1",
            "mapped-parent",
            "candidate-2",
        ))
        .await
        .unwrap(),
        DeliveryTransitionResult::Applied(_)
    ));
    assert_eq!(
        repo.get_delivery(&id(1)).await.unwrap().unwrap().state,
        TaskDeliveryState::Superseded
    );
    assert_eq!(
        repo.get_delivery(&id(2)).await.unwrap().unwrap().state,
        TaskDeliveryState::Prepared
    );
}

#[tokio::test]
async fn mapped_head_retry_refuses_unmapped_or_changed_source_without_mutation() {
    let (db, repo) = fixture().await;
    seed_mapped_parent(&db, &repo).await;
    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    for retry in [
        mapped_head_retry("unmapped", "source-1", "unmapped-parent", "candidate-2"),
        mapped_head_retry("changed-source", "source-2", "mapped-parent", "candidate-2"),
    ] {
        assert!(matches!(
            repo.retry_delivery_from_mapped_head(&retry).await.unwrap(),
            DeliveryTransitionResult::Stale { .. }
        ));
    }
    assert_eq!(
        repo.get_delivery(&id(1)).await.unwrap().unwrap().state,
        TaskDeliveryState::Prepared
    );
    assert!(repo.get_delivery(&id(2)).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM task_deliveries WHERE build_attempt_id = 'attempt' AND task_id = 'task'"
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_mapped_head_generation_cas_has_exactly_one_winner() {
    let (db, repo) = fixture().await;
    seed_mapped_parent(&db, &repo).await;
    repo.prepare_delivery(&prepare(1, "source-1", "candidate-1"))
        .await
        .unwrap();
    let left = mapped_head_retry("race-left", "source-1", "mapped-parent", "candidate-left");
    let right = mapped_head_retry("race-right", "source-1", "mapped-parent", "candidate-right");
    let (left, right) = tokio::join!(
        repo.retry_delivery_from_mapped_head(&left),
        repo.retry_delivery_from_mapped_head(&right)
    );
    let results = [left.unwrap(), right.unwrap()];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, DeliveryTransitionResult::Applied(_)))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, DeliveryTransitionResult::Stale { .. }))
            .count(),
        1
    );
    let new = repo.get_delivery(&id(2)).await.unwrap().unwrap();
    assert!(matches!(
        new.candidate_sha.as_str(),
        "candidate-left" | "candidate-right"
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM task_deliveries WHERE build_attempt_id = 'attempt' AND task_id = 'task' AND state IN ('prepared', 'applying')"
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        1
    );
}
