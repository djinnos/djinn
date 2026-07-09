//! Tests for the autonomous-escalation ceiling on the loop-breaker ladder.
//!
//! Below the ceiling, a loop-breaker creates a `planner-park-escalation` and
//! parks the source. Once `MAX_AUTONOMOUS_ESCALATIONS` escalations have been
//! spent on a source, the next loop-breaker terminally fails (ForceClose) the
//! source instead of parking it for a human — the no-human bottom of the
//! ladder. No path ever produces a human-review hold.

use super::*;

/// Create a closed `planner-park-escalation` review task that blocks `source`,
/// simulating one prior autonomous escalation round.
async fn seed_prior_escalation(repo: &TaskRepository, source: &djinn_core::models::Task, n: usize) {
    let escalation = repo
        .create_in_project(
            &source.project_id,
            None,
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
    seed_prior_escalation(&repo, &task, 1).await;

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
        seed_prior_escalation(&repo, &task, n).await;
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
