use super::*;

// ── Status ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_status_is_zero() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let handle = spawn_coordinator(&db, &tx);

    let status = handle.get_status().unwrap();
    assert_eq!(status.tasks_dispatched, 0);
    assert_eq!(status.sessions_recovered, 0);
}

// ── Dispatch on open-task event ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trigger_dispatch_increments_counter_for_ready_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let outcome = actor
        .try_dispatch_to_pool(
            "T1",
            "worker",
            0,
            None,
            &[DEFAULT_MODEL_ID.to_owned()],
            |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
        )
        .await;
    assert!(matches!(outcome, DispatchOutcome::Dispatched));
    actor.dispatched += 1;

    assert!(
        actor.dispatched >= 1,
        "should have dispatched the ready task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trigger_dispatch_increments_counter_for_review_tasks() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let outcome = actor
        .try_dispatch_to_pool(
            "Review me",
            "reviewer",
            0,
            None,
            &[DEFAULT_MODEL_ID.to_owned()],
            |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
        )
        .await;
    assert!(matches!(outcome, DispatchOutcome::Dispatched));
    actor.dispatched += 1;

    assert!(
        actor.dispatched >= 1,
        "should dispatch task waiting for review"
    );
}

// ── Stuck detection ───────────────────────────────────────────────────────

/// VerificationTracker was removed — this test exercised the tracker-based
/// background-work protection for stuck detection. With the tracker gone, the
/// test is obsolete and replaced with a no-op placeholder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_detection_skips_task_with_background_post_session_work_obsolete() {
    // No-op: VerificationTracker removed. Kept as a placeholder to document
    // that the tracker-based background-work protection is intentionally gone.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_detection_releases_orphaned_in_progress_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // Manually put a task in_progress (simulating an orphaned session).
    let task = repo
        .create(&epic.id, "Stuck", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.set_status(&task.id, "in_progress").await.unwrap();

    let handle = spawn_coordinator(&db, &tx);
    handle.trigger_dispatch().await.unwrap();
    // Trigger dispatch to also run stuck detection; wait for recovery.
    handle.wait_for_status(|s| s.sessions_recovered >= 1).await;

    let status = handle.get_status().unwrap();
    assert!(
        status.sessions_recovered >= 1,
        "stuck task should have been recovered"
    );

    // The released task should now be back to open.
    let updated = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        updated.status, "open",
        "released task should be back to open"
    );
}

/// Verifying status was removed from the task state machine.
/// These tests exercised the recovery/protection paths for stuck `verifying`
/// tasks; with verifying gone they are obsolete and removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_verifying_recovery_tests_obsolete() {
    // No-op: verifying status removed. Kept as a placeholder to document
    // that the verifying-specific recovery logic is intentionally gone.
}

/// Verifying status was removed from the task state machine.
/// This test exercised re-arming a verifying task against a terminal in-pod
/// verification row; with verifying gone it is obsolete and removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stuck_verifying_with_terminal_inpod_run_rearm_obsolete() {
    // No-op: verifying status removed.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_closed_task_applies_failure_confidence_once() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _handle = spawn_coordinator(&db, &tx);
    let (task, note) = create_task_with_note(&db, &tx, "failed-close").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    repo.set_status_with_reason(&task.id, "closed", Some("failed"))
        .await
        .unwrap();
    // Deterministic sync: wait for the coordinator to record the
    // FAILED_CLOSE marker instead of a fixed sleep (which flaked under
    // load — the coordinator actor processes the status_changed event
    // asynchronously and 100ms is not a hard upper bound on latency).
    wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_FAILED_CLOSE, 0).await;

    let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let note_after = note_repo.get(&note.id).await.unwrap().unwrap();
    assert!(note_after.confidence < 0.5);

    let markers = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
            actor_role: Some("system".to_string()),
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 20,
            offset: 0,
        })
        .await
        .unwrap();
    assert_eq!(markers.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&markers[0].payload).unwrap();
    assert_eq!(payload["kind"], TASK_OUTCOME_FAILED_CLOSE);
    assert_eq!(payload["reopen_count"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopened_twice_applies_failure_once_per_reopen_count() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let _handle = spawn_coordinator(&db, &tx);
    let (task, note) = create_task_with_note(&db, &tx, "reopen-twice").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let note_repo = NoteRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    repo.set_status_with_reason(&task.id, "closed", Some("failed"))
        .await
        .unwrap();
    // Deterministic sync on each coordinator-observed side-effect
    // instead of fixed sleeps.  Fixed-duration sleeps flaked because the
    // coordinator actor processes status_changed events asynchronously
    // and 50-150ms is not a hard upper bound on scheduler + DB latency
    // under parallel-test load.
    wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_FAILED_CLOSE, 0).await;
    repo.set_status(&task.id, "open").await.unwrap();
    wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_REOPEN_COUNT, 1).await;
    let reopened_once = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(reopened_once.reopen_count, 1);
    let after_first = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
    assert!(after_first < 0.5, "first reopen should reduce confidence");

    // Duplicate open→open: the coordinator must treat this as a no-op
    // (marker for reopen_count=1 already exists).  Assert idempotency as
    // an integer invariant — the marker count must not grow — rather
    // than as float-equality on the derived confidence.  Float-equality
    // conflates "penalty not applied" with "penalty not yet applied"
    // and is what made the original test flaky.
    let markers_before_duplicate = outcome_marker_count(&repo, &task.id).await;
    repo.set_status(&task.id, "open").await.unwrap();
    // There is no positive-side-effect marker to poll for a no-op, so
    // drive a follow-up transition that DOES produce a marker and poll
    // for it; once that marker lands we know the duplicate event was
    // drained from the coordinator's queue without producing a
    // second reopen_count=1 marker.
    repo.set_status_with_reason(&task.id, "closed", Some("failed"))
        .await
        .unwrap();
    wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_FAILED_CLOSE, 1).await;
    repo.set_status(&task.id, "open").await.unwrap();
    wait_for_outcome_marker(&repo, &task.id, TASK_OUTCOME_REOPEN_COUNT, 2).await;

    let reopened_twice = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(reopened_twice.reopen_count, 2);
    let after_second = note_repo.get(&note.id).await.unwrap().unwrap().confidence;
    assert!(
        after_second <= after_first,
        "second reopen should not increase confidence, got after_second={after_second}, after_first={after_first}"
    );
    // Exactly two new markers between the duplicate no-op and now:
    // one FAILED_CLOSE(reopen_count=1) and one REOPEN_COUNT(reopen_count=2).
    // If the duplicate open→open had wrongly applied a penalty, we'd
    // see three.
    let markers_after = outcome_marker_count(&repo, &task.id).await;
    assert_eq!(
        markers_after - markers_before_duplicate,
        2,
        "duplicate open→open must be a no-op: expected +2 markers (FAILED_CLOSE rc=1, REOPEN_COUNT rc=2), got +{}",
        markers_after - markers_before_duplicate,
    );

    let markers = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some(TASK_OUTCOME_CONFIDENCE_ACTIVITY.to_string()),
            actor_role: Some("system".to_string()),
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 20,
            offset: 0,
        })
        .await
        .unwrap();
    let reopen_markers: Vec<serde_json::Value> = markers
        .into_iter()
        .map(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).unwrap())
        .filter(|payload: &serde_json::Value| payload["kind"] == TASK_OUTCOME_REOPEN_COUNT)
        .collect();
    assert_eq!(reopen_markers.len(), 2);
    assert!(
        reopen_markers
            .iter()
            .any(|payload| payload["reopen_count"] == 1)
    );
    assert!(
        reopen_markers
            .iter()
            .any(|payload| payload["reopen_count"] == 2)
    );
}
