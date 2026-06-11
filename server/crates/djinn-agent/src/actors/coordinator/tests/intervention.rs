use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_threshold_does_not_intervene() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD - 1).await;

    let intervened = actor.maybe_intervene_on_stuck_task(&task).await;
    assert!(!intervened, "below threshold must not intervene");

    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    assert!(
        planner_intervention_markers(&repo, &task.id)
            .await
            .is_empty(),
        "no intervention marker should be written below threshold"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_threshold_routes_to_planner_intervention() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;

    let intervened = actor.maybe_intervene_on_stuck_task(&task).await;
    assert!(
        intervened,
        "at threshold must route to planner intervention"
    );

    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // Exactly one intervention marker, keyed to the current reopen count.
    let markers = planner_intervention_markers(&repo, &task.id).await;
    assert_eq!(markers.len(), 1, "exactly one intervention marker");
    assert_eq!(markers[0]["reopen_count"], REOPEN_INTERVENTION_THRESHOLD);

    // A Planner review task was created in the same project.
    let reviews = repo.list_by_status("open").await.unwrap();
    assert!(
        reviews
            .iter()
            .any(|t| t.issue_type == "review" && t.project_id == task.project_id),
        "a review (planner intervention) task must be created"
    );

    // The source task carries a PLANNER_ESCALATION comment linking it.
    let comments = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("comment".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 100,
            offset: 0,
        })
        .await
        .unwrap();
    assert!(
        comments
            .iter()
            .any(|c| c.payload.contains("PLANNER_ESCALATION")),
        "source task must record a PLANNER_ESCALATION comment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intervention_is_idempotent_per_reopen_count() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // First pass intervenes; subsequent passes at the SAME reopen count are
    // suppressed by the marker — no Planner storm while one is in flight.
    assert!(actor.maybe_intervene_on_stuck_task(&task).await);
    assert!(!actor.maybe_intervene_on_stuck_task(&task).await);
    assert!(!actor.maybe_intervene_on_stuck_task(&task).await);

    assert_eq!(
        planner_intervention_markers(&repo, &task.id).await.len(),
        1,
        "idempotent: a single marker for one reopen-count value"
    );

    // A genuine new reopen (count bumps past threshold again) re-arms one
    // fresh intervention.
    repo.set_status(&task.id, "closed").await.unwrap();
    let bumped = repo.set_status(&task.id, "open").await.unwrap();
    assert_eq!(bumped.reopen_count, REOPEN_INTERVENTION_THRESHOLD + 1);

    assert!(
        actor.maybe_intervene_on_stuck_task(&bumped).await,
        "a higher reopen count must re-arm intervention"
    );
    assert_eq!(
        planner_intervention_markers(&repo, &task.id).await.len(),
        2,
        "one marker per distinct reopen-count value"
    );
}

/// Second strike: once the Planner has already intervened
/// (`intervention_count >= MAX_PLANNER_INTERVENTIONS`) and the task has
/// STILL climbed back to the reopen threshold, the coordinator parks it
/// terminally instead of escalating to the Planner again — no new marker,
/// no new review task, and the task ends up `closed`. This is the loop
/// breaker for the txr4 case (rescope didn't help → stop hogging the slot).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_strike_parks_task_after_prior_intervention() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // Reach the threshold once, simulate a completed planner intervention
    // (bumps intervention_count, resets reopen_count), then climb back to the
    // threshold a second time.
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
    repo.reset_intervention_counters(&task.id).await.unwrap();
    for _ in 0..REOPEN_INTERVENTION_THRESHOLD {
        repo.set_status(&task.id, "closed").await.unwrap();
        repo.set_status(&task.id, "open").await.unwrap();
    }
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.intervention_count, 1, "one prior planner intervention");
    assert_eq!(task.reopen_count, REOPEN_INTERVENTION_THRESHOLD);

    let handled = actor.maybe_intervene_on_stuck_task(&task).await;
    assert!(
        handled,
        "second strike must handle the task (caller skips worker dispatch)"
    );

    // Parked terminally — task is closed.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "closed",
        "second strike force-closes the task"
    );

    // No planner intervention marker for this reopen count, and no new
    // planner review task — the loop is broken, not re-escalated.
    assert!(
        !planner_intervention_markers(&repo, &task.id)
            .await
            .iter()
            .any(|m| m["reopen_count"] == REOPEN_INTERVENTION_THRESHOLD),
        "second strike must not write a new planner intervention marker"
    );
    let reviews = repo.list_by_status("open").await.unwrap();
    assert!(
        !reviews
            .iter()
            .any(|t| t.issue_type == "review" && t.project_id == parked.project_id),
        "second strike must not create another planner review task"
    );
}
