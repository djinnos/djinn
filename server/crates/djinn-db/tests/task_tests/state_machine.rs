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
async fn board_health_report() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create tasks: one open, one in_progress.
    let _t1 = open_task(&repo, &epic.id).await;
    let t2 = open_task(&repo, &epic.id).await;
    repo.transition(&t2.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let epic_stats = report["epic_stats"].as_array().unwrap();
    assert_eq!(epic_stats.len(), 1);
    assert_eq!(epic_stats[0]["total"], 2);
    assert!(report.get("memory_health").is_none());

    // Backdate t2's updated_at to simulate staleness.
    let t2_id = t2.id.clone();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = $1")
        .bind(&t2_id)
        .execute(db.pool())
        .await
        .unwrap();

    let report2 = repo.board_health(24).await.unwrap();
    let stale = report2["stale_tasks"].as_array().unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0]["short_id"], t2.short_id.as_str());
    assert!(report2.get("memory_health").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_flags_repeated_reopen_role_tool_mismatch_candidates() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Plan next wave after repeated worker churn",
            "Repeated reopen churn suggests this should create planning tasks instead of more worker implementation.",
            "Use task_create to split work and epic_update to refresh epic metadata.",
            "task",
            1,
            "planner",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET total_reopen_count = 3 WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    let _session = create_test_session(&db, &project.id, &task.id).await;

    let report = repo.board_health(24).await.unwrap();
    let mismatches = report
        .get("repeated_reopen_role_tool_mismatches")
        .and_then(|v| v.as_array())
        .expect("repeated_reopen_role_tool_mismatches field should exist");
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0]["short_id"], task.short_id.as_str());
    assert_eq!(mismatches[0]["dispatched_role"], "worker");
    assert_eq!(mismatches[0]["expected_role"], "planner");
    assert_eq!(mismatches[0]["total_reopen_count"], 3);
    assert_eq!(mismatches[0]["session_count"], 1);
    assert_eq!(
        mismatches[0]["mismatch_signals"],
        serde_json::json!([
            "requires:task_create",
            "requires:epic_update",
            "requires:planning"
        ])
    );
    assert_eq!(
        mismatches[0]["reason"],
        "Repeated reopen churn (3 reopens) suggests this task needs the planner toolset (task_create, epic_update, memory_ref_update, reprioritization) rather than the currently routed worker toolset (code_changes, tests, review_fix)."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_ignores_repeated_reopen_tasks_without_role_tool_mismatch() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let task = repo
        .create_in_project(
            &epic.project_id,
            Some(&epic.id),
            "Implement worker-safe fix",
            "A normal implementation task with code changes only.",
            "Edit Rust code and update tests in the existing module.",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET total_reopen_count = 4 WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let mismatches = report
        .get("repeated_reopen_role_tool_mismatches")
        .and_then(|v| v.as_array())
        .expect("repeated_reopen_role_tool_mismatches field should exist");
    assert!(mismatches.is_empty());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_returns_liveness_outcomes_section() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create a task + session + liveness evidence row.
    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Liveness task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;

    let evidence_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO liveness_evidence \
         (id, session_id, task_id, verdict, outcome_kind, evidence, created_at) \
         VALUES ($1, $2, $3, 'dead', 'dead_reclaimed', '{}', \
                 '2025-06-01T00:00:00.000Z')",
    )
    .bind(&evidence_id)
    .bind(&session.id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let liveness = report
        .get("liveness_outcomes")
        .expect("liveness_outcomes field should exist");
    assert_eq!(liveness["total"], 1);
    let by_verdict = liveness["by_verdict"].as_object().unwrap();
    assert_eq!(by_verdict["dead"], 1);
    let recent = liveness["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["verdict"], "dead");
    assert_eq!(recent[0]["outcome_kind"], "dead_reclaimed");
    assert_eq!(recent[0]["task_id"], task.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_returns_protocol_violations_section() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Protocol violation task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;

    let evidence_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO liveness_evidence \
         (id, session_id, task_id, verdict, outcome_kind, outcome_reason, evidence, created_at) \
         VALUES ($1, $2, $3, 'protocol_violation', 'protocol_violation', \
                 'clean_exit_nonterminal', '{}', '2025-06-01T00:00:00.000Z')",
    )
    .bind(&evidence_id)
    .bind(&session.id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let pv = report
        .get("protocol_violations")
        .expect("protocol_violations field should exist");
    assert_eq!(pv["total"], 1);
    let recent = pv["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["verdict"], "protocol_violation");
    assert_eq!(recent[0]["outcome_reason"], "clean_exit_nonterminal");
    assert_eq!(recent[0]["task_short_id"], task.short_id.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_detects_stale_open_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create an open task (no sessions) and backdate it so it exceeds the
    // 30-minute stranded-ready threshold.
    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Starved open task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let sr = report
        .get("stranded_ready")
        .expect("stranded_ready field should exist");
    assert_eq!(sr["total"], 1);
    assert_eq!(sr["threshold_minutes"], 30);
    let findings = sr["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["id"], task.id);
    assert_eq!(findings[0]["short_id"], task.short_id.as_str());
    assert_eq!(findings[0]["severity"], "error"); // 60m ≥ 2×30 error threshold
    assert_eq!(findings[0]["unclaimed_since_confidence"], "low"); // no activity log → fallback
    assert_eq!(findings[0]["threshold"]["warning_minutes"], 30);
    assert_eq!(findings[0]["threshold"]["error_minutes"], 60);
    assert_eq!(findings[0]["threshold"]["critical_minutes"], 180);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_tasks_with_active_sessions() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create an open task with an active running session.
    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Active task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;
    // Mark session as running.
    sqlx::query("UPDATE sessions SET status = 'running' WHERE id = $1")
        .bind(&session.id)
        .execute(db.pool())
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let sr = report
        .get("stranded_ready")
        .expect("stranded_ready field should exist");
    let findings = sr["findings"].as_array().unwrap();
    assert!(
        findings.is_empty(),
        "task with active session should not appear in stranded_ready"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_high_confidence_with_activity_log() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create an open task and insert an activity_log entry showing it was
    // transitioned to 'open' long ago.
    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Task with activity log",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    // Insert a 'status_changed' → 'open' activity log entry backdated by 2 hours.
    let activity_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO activity_log \
         (id, task_id, actor_id, actor_role, event_type, payload, created_at) \
         VALUES ($1, $2, 'system', 'system', 'status_changed', \
                 '{\"from_status\": \"in_progress\", \"to_status\": \"open\"}', \
                 '2025-01-01T00:00:00.000Z')",
    )
    .bind(&activity_id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let sr = report
        .get("stranded_ready")
        .expect("stranded_ready field should exist");
    let findings = sr["findings"].as_array().unwrap();
    // At least one finding for this task.
    let finding = findings
        .iter()
        .find(|f| f["id"] == task.id)
        .expect("task should appear in stranded_ready");
    assert_eq!(finding["unclaimed_since_confidence"], "high");
    assert_eq!(finding["unclaimed_since"], "2025-01-01T00:00:00.000Z");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_severity_escalates_with_elapsed_time() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Task at 35 minutes: warning (≥1×30, <2×30).
    let t1 = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Warning task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &t1.id, "35 minutes").await;

    // Task at 65 minutes: error (≥2×30, <6×30).
    let t2 = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Error task",
            "desc",
            "design",
            "task",
            2,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &t2.id, "65 minutes").await;

    // Task at 200 minutes: critical (≥6×30).
    let t3 = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Critical task",
            "desc",
            "design",
            "task",
            3,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &t3.id, "200 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();

    let f1 = findings.iter().find(|f| f["id"] == t1.id).unwrap();
    assert_eq!(f1["severity"], "warning");

    let f2 = findings.iter().find(|f| f["id"] == t2.id).unwrap();
    assert_eq!(f2["severity"], "error");

    let f3 = findings.iter().find(|f| f["id"] == t3.id).unwrap();
    assert_eq!(f3["severity"], "critical");
}

/// A stale ready task whose dispatch backoff ladder has tripped
/// (`failure_streak >= 3`) is an explicit rate-limit gate, so it must be
/// excluded from stranded_ready rather than reported as starved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_rate_limited_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Rate-limited task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;
    // Rate-limit backoff has tripped (>= 3 consecutive dispatch failures).
    sqlx::query("INSERT INTO dispatch_state (task_id, failure_streak) VALUES ($1, 3)")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "rate-limited task (failure_streak >= 3) must be excluded from stranded_ready"
    );
}

/// A stale ready task with a future breaker cooldown is intentionally held, so
/// it must be excluded from stranded_ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_cooldown_active_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Cooldown task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;
    // Breaker cooldown deadline one hour in the future.
    sqlx::query(
        "INSERT INTO dispatch_state (task_id, cooldown_until) \
         VALUES ($1, now() AT TIME ZONE 'utc' + interval '1 hour')",
    )
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "breaker-open (future cooldown) task must be excluded from stranded_ready"
    );
}

/// A stale ready task with a paused session is manually held, so it must be
/// excluded from stranded_ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_paused_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Paused task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    let session = create_test_session(&db, &project.id, &task.id).await;
    sqlx::query("UPDATE sessions SET status = 'paused' WHERE id = $1")
        .bind(&session.id)
        .execute(db.pool())
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "manually paused task must be excluded from stranded_ready"
    );
}

/// A genuinely stranded task carries a fully-populated `dispatch_gate` object.
/// When the inflight model has no healthy model_health row, `image_ready` is
/// false, `no_eligible_model` is reported, and the gate verdict is `blocked`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_gate_evidence_fields() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Gate evidence task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;
    // An inflight model was chosen, but there is no model_health row for it.
    sqlx::query(
        "INSERT INTO dispatch_state (task_id, failure_streak, last_dispatched_role, inflight_model_id) \
         VALUES ($1, 0, 'worker', 'testprov/testmodel')",
    )
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    let finding = findings
        .iter()
        .find(|f| f["id"] == task.id)
        .expect("task should appear as stranded");
    let gate = &finding["dispatch_gate"];
    assert_eq!(gate["evaluated_role"], "worker");
    assert!(gate["toolset"].as_array().is_some_and(|t| !t.is_empty()));
    assert_eq!(gate["model_requirement"], "testprov/testmodel");
    assert_eq!(gate["image_ready"], false);
    assert_eq!(gate["breaker_open"], false);
    assert_eq!(gate["manually_paused"], false);
    assert_eq!(gate["rate_limited"], false);
    assert_eq!(gate["credential_available"], true);
    assert_eq!(gate["gate_verdict"], "blocked");
    let reasons = gate["reasons"].as_array().unwrap();
    assert!(
        reasons.iter().any(|r| r == "no_eligible_model"),
        "expected no_eligible_model in dispatch_gate reasons, got {reasons:?}"
    );
}

/// A stale ready task whose creator has credentials that are all revoked (and
/// no org-shared fallback) is owner-credential-blocked and must be excluded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_stranded_ready_excludes_credential_revoked_tasks() {
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Credential-blocked task",
            "desc",
            "design",
            "task",
            1,
            "worker",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    backdate_task_updated_at(&db, &task.id, "60 minutes").await;

    // Attribute the task to a user whose only credential is revoked.
    let user_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, github_id, github_login) VALUES ($1, $2, $3)")
        .bind(&user_id)
        .bind(rand_github_id())
        .bind(format!("user-{user_id}"))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET created_by_user_id = $1 WHERE id = $2")
        .bind(&user_id)
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    let cred_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO credentials \
         (id, provider_id, key_name, encrypted_value, owner_user_id, revoked_at) \
         VALUES ($1, 'anthropic', $2, '\\x00'::bytea, $3, '2025-01-01T00:00:00.000Z')",
    )
    .bind(&cred_id)
    .bind(format!("key-{cred_id}"))
    .bind(&user_id)
    .execute(db.pool())
    .await
    .unwrap();

    let report = repo.board_health(24).await.unwrap();
    let findings = report["stranded_ready"]["findings"].as_array().unwrap();
    assert!(
        findings.iter().all(|f| f["id"] != task.id),
        "owner-credential-revoked task must be excluded from stranded_ready"
    );
}

/// Small unique-ish github_id generator for seeding test users without
/// colliding on the `uq_users_github_id` constraint within a test DB. Derived
/// from a fresh UUIDv7 (monotonic, high-entropy) to stay off wall-clock APIs.
fn rand_github_id() -> i64 {
    (uuid::Uuid::now_v7().as_u128() & 0x7fff_ffff_ffff) as i64
}
