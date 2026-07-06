use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_enum_roundtrips() {
    let statuses = [
        "open",
        "in_progress",
        "needs_task_review",
        "in_task_review",
        "approved",
        "pr_draft",
        "pr_review",
        "closed",
    ];
    for s in statuses {
        let parsed = TaskStatus::parse(s).unwrap();
        assert_eq!(parsed.as_str(), s, "round-trip failed for {s}");
    }
    assert!(TaskStatus::parse("unknown").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_happy_path() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Tasks are created as "open".
    let task = open_task(&repo, &epic.id).await;
    assert_eq!(task.status, "open");

    // start
    let t = repo
        .transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();
    assert_eq!(t.status, "in_progress");

    // submit_task_review
    let t = repo
        .transition(
            &t.id,
            TransitionAction::SubmitTaskReview,
            "",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(t.status, "needs_task_review");

    // task_review_start
    let t = repo
        .transition(
            &t.id,
            TransitionAction::TaskReviewStart,
            "",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(t.status, "in_task_review");

    // task_review_approve closes the task.
    let t = repo
        .transition(
            &t.id,
            TransitionAction::TaskReviewApprove,
            "",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(t.status, "approved");
    assert!(t.closed_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_transition_returns_error() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;

    // Can't submit_task_review from open (must be in_progress).
    let err = repo
        .transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "",
            "system",
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::InvalidTransition(_)),
        "expected InvalidTransition, got {err:?}"
    );

    // Can't submit_task_review from open (must be in_progress).
    let err = repo
        .transition(
            &task.id,
            TransitionAction::SubmitTaskReview,
            "",
            "system",
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_review_reject_increments_reopen() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    let t = repo
        .transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();
    let t = repo
        .transition(
            &t.id,
            TransitionAction::SubmitTaskReview,
            "",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    let t = repo
        .transition(
            &t.id,
            TransitionAction::TaskReviewStart,
            "",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();

    let t = repo
        .transition(
            &t.id,
            TransitionAction::TaskReviewReject,
            "reviewer@example.com",
            "reviewer",
            Some("needs more tests"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(t.status, "open");
    assert_eq!(t.reopen_count, 1);
    assert_eq!(t.continuation_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_review_reject_conflict_does_not_increment_reopen() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    let t = repo
        .transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();
    let t = repo
        .transition(
            &t.id,
            TransitionAction::SubmitTaskReview,
            "",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    let t = repo
        .transition(
            &t.id,
            TransitionAction::TaskReviewStart,
            "",
            "reviewer",
            None,
            None,
        )
        .await
        .unwrap();

    let t = repo
        .transition(
            &t.id,
            TransitionAction::TaskReviewRejectConflict,
            "reviewer@example.com",
            "reviewer",
            Some("merge conflict"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(t.status, "open");
    assert_eq!(t.reopen_count, 0); // conflict doesn't count against budget
    assert_eq!(t.continuation_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_close_from_any_state() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    let t = repo
        .transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();

    let t = repo
        .transition(
            &t.id,
            TransitionAction::ForceClose,
            "admin",
            "user",
            Some("cancelled"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(t.status, "closed");
    assert!(t.closed_at.is_some());
    assert_eq!(t.close_reason.as_deref(), Some("force_closed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_clears_closed_at_and_increments_reopen() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    // Force-close it directly.
    let t = repo
        .transition(
            &task.id,
            TransitionAction::ForceClose,
            "admin",
            "user",
            Some("testing"),
            None,
        )
        .await
        .unwrap();
    assert!(t.closed_at.is_some());
    assert_eq!(t.close_reason.as_deref(), Some("force_closed"));

    // Reopen.
    let t = repo
        .transition(
            &t.id,
            TransitionAction::Reopen,
            "user",
            "user",
            Some("still needed"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(t.status, "open");
    assert!(t.closed_at.is_none());
    assert!(t.close_reason.is_none());
    assert_eq!(t.reopen_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_blocked_when_acceptance_criteria_empty() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;

    // Explicitly set AC to empty array; start should be rejected.
    let updated = repo
        .update(
            &task.id,
            &task.title,
            &task.description,
            &task.design,
            task.priority,
            &task.owner,
            &task.labels,
            "[]",
        )
        .await
        .unwrap();
    assert_eq!(updated.acceptance_criteria, "[]");

    let err = repo
        .transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::InvalidTransition(msg) if msg == "task has no acceptance criteria")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_allows_when_acceptance_criteria_present() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;

    let updated = repo
        .update(
            &task.id,
            &task.title,
            &task.description,
            &task.design,
            task.priority,
            &task.owner,
            &task.labels,
            r#"[{"criterion":"can start"}]"#,
        )
        .await
        .unwrap();
    assert_ne!(updated.acceptance_criteria, "[]");

    let started = repo
        .transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();
    assert_eq!(started.status, "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_allows_planning_without_acceptance_criteria() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Planning tasks have no AC by design — the planner produces the breakdown.
    let task = repo
        .create(
            &epic.id,
            "Plan next wave",
            "",
            "",
            "planning",
            0,
            "",
            Some("open"),
        )
        .await
        .unwrap();
    assert_eq!(task.acceptance_criteria, "[]");

    let started = repo
        .transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();
    assert_eq!(started.status, "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_blocked_by_unresolved_blockers() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let t1 = open_task(&repo, &epic.id).await;
    let t2 = open_task(&repo, &epic.id).await;
    repo.add_blocker(&t2.id, &t1.id).await.unwrap(); // t2 blocked by t1

    let err = repo
        .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition(_)));

    // After removing the blocker, start succeeds.
    repo.remove_blocker(&t2.id, &t1.id).await.unwrap();
    let t = repo
        .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();
    assert_eq!(t.status, "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_allowed_when_blocker_is_closed() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let blocker = open_task(&repo, &epic.id).await;
    let blocked = open_task(&repo, &epic.id).await;
    repo.add_blocker(&blocked.id, &blocker.id).await.unwrap();

    // Closed blockers are considered resolved.
    repo.set_status(&blocker.id, "closed").await.unwrap();

    let started = repo
        .transition(
            &blocked.id,
            TransitionAction::Start,
            "",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(started.status, "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_override_to_closed() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    let t = repo
        .transition(
            &task.id,
            TransitionAction::UserOverride,
            "admin",
            "user",
            None,
            Some(TaskStatus::Closed),
        )
        .await
        .unwrap();

    assert_eq!(t.status, "closed");
    assert!(t.closed_at.is_some());
    assert_eq!(t.close_reason.as_deref(), Some("force_closed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requires_reason_enforced() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    // ForceClose requires a reason.
    let err = repo
        .transition(
            &task.id,
            TransitionAction::ForceClose,
            "",
            "user",
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition(_)));

    // With a reason it works.
    let t = repo
        .transition(
            &task.id,
            TransitionAction::ForceClose,
            "",
            "user",
            Some("testing"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_writes_activity_log() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    repo.transition(
        &task.id,
        TransitionAction::Start,
        "agent-1",
        "system",
        None,
        None,
    )
    .await
    .unwrap();

    let entries = repo.list_activity(&task.id).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].event_type, "status_changed");
    assert_eq!(entries[0].actor_id, "agent-1");

    let payload: serde_json::Value = serde_json::from_str(&entries[0].payload).unwrap();
    assert_eq!(payload["from_status"], "open");
    assert_eq!(payload["to_status"], "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_activity_filters() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let t1 = open_task(&repo, &epic.id).await;
    let t2 = open_task(&repo, &epic.id).await;

    // Log a comment on t1 and a status_changed on t2.
    repo.log_activity(Some(&t1.id), "u1", "user", "comment", r#"{"body":"hello"}"#)
        .await
        .unwrap();
    repo.log_activity(
        Some(&t2.id),
        "sys",
        "system",
        "status_changed",
        r#"{"from":"open"}"#,
    )
    .await
    .unwrap();

    // Filter by task_id.
    let results = repo
        .query_activity(ActivityQuery {
            task_id: Some(t1.id.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].event_type, "comment");

    // Filter by event_type across all tasks.
    let results = repo
        .query_activity(ActivityQuery {
            event_type: Some("status_changed".to_owned()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].task_id.as_deref(), Some(t2.id.as_str()));

    // No filters — returns both.
    let all = repo.query_activity(ActivityQuery::default()).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_merge_commit_sha_persists_value() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    let updated = repo
        .set_merge_commit_sha(&task.id, "0123456789abcdef0123456789abcdef01234567")
        .await
        .unwrap();

    assert_eq!(
        updated.merge_commit_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_heals_stale_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let t = open_task(&repo, &epic.id).await;
    repo.transition(&t.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();

    // Backdate updated_at so the task is considered stale (> 24h).
    let t_id = t.id.clone();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = $1")
        .bind(&t_id)
        .execute(db.pool())
        .await
        .unwrap();

    let result = repo.reconcile(24).await.unwrap();
    assert_eq!(result["healed_tasks"], 1);

    // Task should now be open again.
    let updated = repo.resolve(&t.id).await.unwrap().unwrap();
    assert_eq!(updated.status, "open");

    // Activity log should have a reconcile_stale entry.
    let entries = repo.list_activity(&t.id).await.unwrap();
    let reconcile_entry = entries.iter().find(|e| {
        let p: serde_json::Value = serde_json::from_str(&e.payload).unwrap_or_default();
        p["reason"] == "reconcile_stale"
    });
    assert!(
        reconcile_entry.is_some(),
        "expected reconcile_stale activity entry"
    );
}
