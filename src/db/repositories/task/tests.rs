use super::*;
use crate::db::repositories::epic::EpicRepository;
use crate::models::task::{TaskStatus, TransitionAction};
use crate::test_helpers;

async fn make_epic(
    db: &Database,
    tx: broadcast::Sender<DjinnEvent>,
) -> crate::models::epic::Epic {
    EpicRepository::new(db.clone(), tx)
        .create("Test Epic", "", "", "", "")
        .await
        .unwrap()
}

async fn open_task(repo: &TaskRepository, epic_id: &str) -> Task {
    repo.create(epic_id, "T", "", "", "task", 0, "")
        .await
        .unwrap()
}

// ── Existing tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn create_and_get_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let task = repo
        .create(&epic.id, "My Task", "", "", "task", 0, "user@example.com")
        .await
        .unwrap();
    assert_eq!(task.title, "My Task");
    assert_eq!(task.status, "open");
    assert_eq!(task.short_id.len(), 4);

    let fetched = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(fetched.title, "My Task");
}

#[tokio::test]
async fn creating_task_reopens_closed_epic() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic_repo = EpicRepository::new(db.clone(), tx.clone());
    let epic = epic_repo.create("Test Epic", "", "", "", "").await.unwrap();
    epic_repo.close(&epic.id).await.unwrap();

    let repo = TaskRepository::new(db.clone(), tx);
    let _task = repo
        .create(&epic.id, "New Task", "", "", "task", 0, "")
        .await
        .unwrap();

    let reopened = epic_repo.get(&epic.id).await.unwrap().unwrap();
    assert_eq!(reopened.status, "open");
    assert!(reopened.closed_at.is_none());
}

#[tokio::test]
async fn short_id_lookup() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "")
        .await
        .unwrap();
    let found = repo.get_by_short_id(&task.short_id).await.unwrap().unwrap();
    assert_eq!(found.id, task.id);
}

#[tokio::test]
async fn create_emits_event() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let _ = rx.recv().await.unwrap(); // consume EpicCreated
    let repo = TaskRepository::new(db, tx);

    repo.create(&epic.id, "Event Task", "", "", "task", 0, "")
        .await
        .unwrap();
    match rx.recv().await.unwrap() {
        DjinnEvent::TaskCreated(t) => assert_eq!(t.title, "Event Task"),
        _ => panic!("expected TaskCreated"),
    }
}

#[tokio::test]
async fn set_status_transitions() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "")
        .await
        .unwrap();
    let updated = repo.set_status(&task.id, "in_progress").await.unwrap();
    assert_eq!(updated.status, "in_progress");

    let closed = repo.set_status(&task.id, "closed").await.unwrap();
    assert_eq!(closed.status, "closed");
    assert!(closed.closed_at.is_some());
}

#[tokio::test]
async fn reopen_increments_counter() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "")
        .await
        .unwrap();
    repo.set_status(&task.id, "closed").await.unwrap();
    let reopened = repo.set_status(&task.id, "open").await.unwrap();
    assert_eq!(reopened.reopen_count, 1);
}

#[tokio::test]
async fn blocker_management() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "")
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "")
        .await
        .unwrap();

    // add blocker: t2 is blocked by t1
    repo.add_blocker(&t2.id, &t1.id).await.unwrap();
    let blockers = repo.list_blockers(&t2.id).await.unwrap();
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].task_id, t1.id);
    assert_eq!(blockers[0].status, "open");
    assert!(!matches!(blockers[0].status.as_str(), "closed"));

    // inverse: t1 blocks t2
    let blocked = repo.list_blocked_by(&t1.id).await.unwrap();
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].task_id, t2.id);

    // self-loop rejected
    assert!(repo.add_blocker(&t1.id, &t1.id).await.is_err());

    // remove blocker
    repo.remove_blocker(&t2.id, &t1.id).await.unwrap();
    assert!(repo.list_blockers(&t2.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn blocker_cycle_detection() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "")
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "")
        .await
        .unwrap();
    let t3 = repo
        .create(&epic.id, "T3", "", "", "task", 2, "")
        .await
        .unwrap();

    // t2 is blocked by t1; t3 is blocked by t2
    repo.add_blocker(&t2.id, &t1.id).await.unwrap();
    repo.add_blocker(&t3.id, &t2.id).await.unwrap();

    // Adding t1 blocked by t3 would create a cycle: t1 → t2 → t3 → t1
    let result = repo.add_blocker(&t1.id, &t3.id).await;
    assert!(result.is_err(), "expected cycle detection to reject this");
}

#[tokio::test]
async fn start_blocked_by_unresolved_blocker() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "")
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "")
        .await
        .unwrap();

    // t2 blocked by t1 (which is open = unresolved)
    repo.add_blocker(&t2.id, &t1.id).await.unwrap();
    let result = repo
        .transition(&t2.id, TransitionAction::Start, "", "system", None, None)
        .await;
    assert!(result.is_err(), "should not start with unresolved blocker");

    // Close t1 → t2 should now be startable
    repo.set_status(&t1.id, "closed").await.unwrap();
    repo.transition(&t2.id, TransitionAction::Start, "", "system", None, None)
        .await
        .expect("should start after blocker resolved");
}

#[tokio::test]
async fn list_ready_excludes_blocked_tasks() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "")
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "")
        .await
        .unwrap();

    // t2 blocked by t1
    repo.add_blocker(&t2.id, &t1.id).await.unwrap();

    let ready = repo.list_ready(ReadyQuery::default()).await.unwrap();
    let ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&t1.id.as_str()), "t1 should be ready");
    assert!(
        !ids.contains(&t2.id.as_str()),
        "t2 should not be ready (blocked)"
    );

    // Close t1 → t2 becomes ready
    repo.set_status(&t1.id, "closed").await.unwrap();
    let ready2 = repo.list_ready(ReadyQuery::default()).await.unwrap();
    let ids2: Vec<&str> = ready2.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ids2.contains(&t2.id.as_str()),
        "t2 should be ready after t1 closed"
    );
}

#[tokio::test]
async fn activity_log() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "")
        .await
        .unwrap();
    repo.log_activity(
        Some(&task.id),
        "user@example.com",
        "user",
        "comment",
        r#"{"body":"hello"}"#,
    )
    .await
    .unwrap();

    let entries = repo.list_activity(&task.id).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].event_type, "comment");
    assert_eq!(entries[0].task_id.as_deref(), Some(task.id.as_str()));
}

#[tokio::test]
async fn list_by_epic() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

    repo.create(&epic.id, "A", "", "", "task", 1, "")
        .await
        .unwrap();
    repo.create(&epic.id, "B", "", "", "feature", 0, "")
        .await
        .unwrap();

    let tasks = repo.list_by_epic(&epic.id).await.unwrap();
    assert_eq!(tasks.len(), 2);
    // Ordered by priority then created_at — B (priority 0) first.
    assert_eq!(tasks[0].title, "B");
}

#[tokio::test]
async fn delete_task_emits_event() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let _ = rx.recv().await.unwrap();
    let repo = TaskRepository::new(db, tx);

    let task = repo
        .create(&epic.id, "Del", "", "", "task", 0, "")
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();

    repo.delete(&task.id).await.unwrap();
    match rx.recv().await.unwrap() {
        DjinnEvent::TaskDeleted { id } => assert_eq!(id, task.id),
        _ => panic!("expected TaskDeleted"),
    }
}

// ── State machine tests ───────────────────────────────────────────────────

#[tokio::test]
async fn status_enum_roundtrips() {
    let statuses = [
        "draft",
        "open",
        "in_progress",
        "needs_task_review",
        "in_task_review",
        "closed",
    ];
    for s in statuses {
        let parsed = TaskStatus::parse(s).unwrap();
        assert_eq!(parsed.as_str(), s, "round-trip failed for {s}");
    }
    assert!(TaskStatus::parse("unknown").is_err());
}

#[tokio::test]
async fn full_happy_path() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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
            "task_reviewer",
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
            "task_reviewer",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(t.status, "closed");
    assert!(t.closed_at.is_some());
    assert_eq!(t.close_reason.as_deref(), Some("completed"));
}

#[tokio::test]
async fn invalid_transition_returns_error() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

    // Can't accept from open (must be draft).
    let err = repo
        .transition(&task.id, TransitionAction::Accept, "", "system", None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidTransition(_)));
}

#[tokio::test]
async fn task_review_reject_increments_reopen() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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
            "task_reviewer",
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
            "task_reviewer",
            Some("needs more tests"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(t.status, "open");
    assert_eq!(t.reopen_count, 1);
    assert_eq!(t.continuation_count, 0);
}

#[tokio::test]
async fn task_review_reject_conflict_does_not_increment_reopen() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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
            "task_reviewer",
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
            "task_reviewer",
            Some("merge conflict"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(t.status, "open");
    assert_eq!(t.reopen_count, 0); // conflict doesn't count against budget
    assert_eq!(t.continuation_count, 0);
}

#[tokio::test]
async fn force_close_from_any_state() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn reopen_clears_closed_at_and_increments_reopen() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn start_blocked_by_unresolved_blockers() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn start_allowed_when_blocker_is_closed() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn user_override_to_closed() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn requires_reason_enforced() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn transition_writes_activity_log() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn query_activity_filters() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn set_merge_commit_sha_persists_value() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db, tx);

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

#[tokio::test]
async fn board_health_report() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), tx.clone());

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

    // Backdate t2's updated_at to simulate staleness.
    let t2_id = t2.id.clone();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
        .bind(&t2_id)
        .execute(db.pool())
        .await
        .unwrap();

    let report2 = repo.board_health(24).await.unwrap();
    let stale = report2["stale_tasks"].as_array().unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0]["short_id"], t2.short_id.as_str());
}

#[tokio::test]
async fn reconcile_heals_stale_tasks() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, tx.clone()).await;
    let repo = TaskRepository::new(db.clone(), tx);

    let t = open_task(&repo, &epic.id).await;
    repo.transition(&t.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();

    // Backdate updated_at so the task is considered stale (> 24h).
    let t_id = t.id.clone();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
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
