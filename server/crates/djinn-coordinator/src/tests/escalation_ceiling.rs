//! Tests for the autonomous-escalation ceiling on the loop-breaker ladder.
//!
//! Below the ceiling, a loop-breaker creates a `planner-park-escalation` and
//! parks the source. Once `MAX_AUTONOMOUS_ESCALATIONS` escalations have been
//! spent on a source, the next loop-breaker terminally fails (ForceClose) the
//! source instead of parking it for a human — the no-human bottom of the
//! ladder. No path ever produces a human-review hold.

use super::*;
use djinn_core::models::{CiStatus, TaskPrCiSnapshotInput};

/// Seed a durable failing required-CI snapshot for `task` and return the
/// reloaded task with the CI projection fields (`ci_status`,
/// `ci_same_signature_count`, `ci_last_remediation_base_sha`, `ci_head_sha`)
/// populated. `baseline == head` models "a remediation already ran against the
/// current head with no new push".
async fn seed_failing_ci_snapshot(
    repo: &TaskRepository,
    task: &djinn_core::models::Task,
    head: &str,
    baseline: Option<&str>,
    same_signature_count: i64,
    status: CiStatus,
) -> djinn_core::models::Task {
    repo.upsert_ci_snapshot(TaskPrCiSnapshotInput {
        task_id: task.id.clone(),
        pr_number: 77,
        head_sha: head.to_owned(),
        ci_status: status,
        blocking_required_check_names: vec!["Quality Gate".to_owned()],
        failure_fingerprint: Some("fp-sig".to_owned()),
        same_signature_count,
        last_remediation_base_sha: baseline.map(str::to_owned),
    })
    .await
    .unwrap();
    repo.get(&task.id).await.unwrap().unwrap()
}

/// The pure dead-end predicate fires only for a worker dispatch of a failing
/// required-CI task whose remediation baseline equals the current head and
/// whose same-signature count has reached the threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_same_signature_deadlock_predicate() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // Wedged: failing CI, baseline == head, count at threshold.
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    let wedged = seed_failing_ci_snapshot(
        &repo,
        &task,
        "HEAD1",
        Some("HEAD1"),
        MAX_AUTONOMOUS_ESCALATIONS, // any value >= threshold (both are 3)
        CiStatus::Failing,
    )
    .await;
    assert!(
        CoordinatorActor::ci_same_signature_deadlocked(&wedged, "worker"),
        "worker + failing + baseline==head + count>=threshold must be a dead-end"
    );
    assert!(
        !CoordinatorActor::ci_same_signature_deadlocked(&wedged, "reviewer"),
        "non-worker roles are never dead-ended by this predicate"
    );

    // Head advanced past the remediation baseline (a new push landed).
    let task2 = make_task_with_reopen_count(&db, &tx, 0).await;
    let advanced = seed_failing_ci_snapshot(
        &repo,
        &task2,
        "HEAD2",
        Some("OLDBASE"),
        5,
        CiStatus::Failing,
    )
    .await;
    assert!(
        !CoordinatorActor::ci_same_signature_deadlocked(&advanced, "worker"),
        "a head that advanced past the remediation baseline is not a dead-end"
    );

    // Below the same-signature threshold.
    let task3 = make_task_with_reopen_count(&db, &tx, 0).await;
    let fresh =
        seed_failing_ci_snapshot(&repo, &task3, "HEAD3", Some("HEAD3"), 1, CiStatus::Failing).await;
    assert!(
        !CoordinatorActor::ci_same_signature_deadlocked(&fresh, "worker"),
        "below the same-signature threshold is not yet a dead-end"
    );

    // Not failing.
    let task4 = make_task_with_reopen_count(&db, &tx, 0).await;
    let green =
        seed_failing_ci_snapshot(&repo, &task4, "HEAD4", Some("HEAD4"), 9, CiStatus::Passing).await;
    assert!(
        !CoordinatorActor::ci_same_signature_deadlocked(&green, "worker"),
        "a non-failing gate is not a dead-end"
    );
}

/// End-to-end: the dispatch ready pass routes a same-signature CI dead-end into
/// the autonomous escalation ladder (planner-park escalation), never a worker
/// respawn and never a human hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_same_signature_deadlock_routes_to_planner_escalation() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    let wedged = seed_failing_ci_snapshot(
        &repo,
        &task,
        "WEDGEDHEAD",
        Some("WEDGEDHEAD"),
        MAX_AUTONOMOUS_ESCALATIONS,
        CiStatus::Failing,
    )
    .await;
    assert_eq!(wedged.status, "open");
    assert!(CoordinatorActor::ci_same_signature_deadlocked(
        &wedged, "worker"
    ));

    actor.dispatch_ready_tasks(Some(&task.project_id)).await;

    // The source was NOT worker-dispatched; it is held by an autonomous
    // planner-park escalation (no human hold).
    assert!(
        !actor.last_dispatched.contains_key(&task.id),
        "dead-end source must not be worker-dispatched"
    );
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        1,
        "dead-end must be held by exactly one escalation blocker"
    );
    let escalation = repo.get(&blockers[0].task_id).await.unwrap().unwrap();
    assert!(
        escalation.labels.contains("planner-park-escalation"),
        "dead-end escalation must be a planner-park escalation; labels={}",
        escalation.labels
    );
    assert!(
        !escalation.labels.contains("human-review-hold"),
        "dead-end escalation must NOT be a human hold; labels={}",
        escalation.labels
    );
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "open",
        "dead-end source is parked open + held"
    );
}

/// Create a closed `planner-park-escalation` review task that blocks `source`,
/// simulating one prior autonomous escalation round.
async fn seed_prior_escalation(
    db: &djinn_db::Database,
    repo: &TaskRepository,
    source: &djinn_core::models::Task,
    n: usize,
) {
    let fixture_identity = uuid::Uuid::now_v7();
    let github_id = (fixture_identity.as_u128() % 9_000_000_000_000_000_000) as i64;
    let user = djinn_db::UserRepository::new(db.clone())
        .upsert_from_github(
            github_id,
            &format!("escalation-ceiling-fixture-{fixture_identity}"),
            Some("Escalation ceiling fixture"),
            None,
        )
        .await
        .expect("persist escalation fixture user");
    let escalation = repo
        .create_in_project_with_provenance(
            &source.project_id,
            None,
            djinn_db::EffectiveCreatorProvenance::explicit_user_id(&user.id),
            &format!(
                "Planner remediation [{}]: prior escalation {n}",
                source.short_id
            ),
            "prior autonomous escalation",
            "resolve autonomously",
            "review",
            0,
            "system",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    repo.update_labels(&escalation.id, r#"["planner-park-escalation"]"#)
        .await
        .unwrap();
    repo.add_blocker(&source.id, &escalation.id).await.unwrap();
    repo.transition(
        &escalation.id,
        djinn_core::models::TransitionAction::Close,
        "planner",
        "system",
        Some("prior escalation resolved"),
        None,
    )
    .await
    .unwrap();
}

/// Below the ceiling: the loop-breaker creates a planner-park escalation and
/// parks the source (no human hold, no terminal close).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_ceiling_creates_planner_escalation_and_parks() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_status(&task.id, "open").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    // One prior escalation already spent — still below the ceiling.
    seed_prior_escalation(&db, &repo, &task, 1).await;

    let parked = actor
        .escalate_to_planner_or_terminally_fail(&task, "loop not converging")
        .await;
    assert!(
        parked,
        "below ceiling must park (return true), not terminally fail"
    );

    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "open",
        "below-ceiling loop-breaker parks the source open"
    );
    assert!(
        after.close_reason.is_none(),
        "a parked source must not carry a close_reason"
    );

    // A fresh, OPEN planner-park escalation now blocks the source.
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    let open_escalations: Vec<_> = {
        let mut v = Vec::new();
        for b in &blockers {
            let t = repo.get(&b.task_id).await.unwrap().unwrap();
            if t.status == "open" && t.labels.contains("planner-park-escalation") {
                v.push(t);
            }
        }
        v
    };
    assert_eq!(
        open_escalations.len(),
        1,
        "exactly one fresh open planner-park escalation must hold the source"
    );
    for b in &blockers {
        let t = repo.get(&b.task_id).await.unwrap().unwrap();
        assert!(
            !t.labels.contains("human-review-hold"),
            "no blocker on the source may be a human-review hold; labels={}",
            t.labels
        );
    }
}

/// At the ceiling: the loop-breaker terminally fails (ForceClose) the source
/// instead of creating another escalation, releasing its blockers. No human
/// hold is ever produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_ceiling_terminally_fails_source_instead_of_parking() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.set_status(&task.id, "open").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();

    // Spend the full escalation ceiling on this source.
    for n in 0..(MAX_AUTONOMOUS_ESCALATIONS as usize) {
        seed_prior_escalation(&db, &repo, &task, n).await;
    }

    let parked = actor
        .escalate_to_planner_or_terminally_fail(&task, "still not converging")
        .await;
    assert!(
        !parked,
        "at the ceiling the loop-breaker must terminally fail (return false), not park"
    );

    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_ne!(
        after.status, "open",
        "at-ceiling loop-breaker must terminally close the source, not re-park it"
    );
    assert!(
        after.close_reason.is_some(),
        "terminal close must record a close_reason"
    );

    // No NEW escalation was created beyond the ceiling: still exactly the
    // MAX_AUTONOMOUS_ESCALATIONS priors, none of them a human hold.
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        MAX_AUTONOMOUS_ESCALATIONS as usize,
        "no fresh escalation must be created past the ceiling"
    );
    for b in &blockers {
        let t = repo.get(&b.task_id).await.unwrap().unwrap();
        assert!(
            !t.labels.contains("human-review-hold"),
            "the exhausted-ladder path must never produce a human hold; labels={}",
            t.labels
        );
    }
}
