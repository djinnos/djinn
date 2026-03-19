use crate::events::{EventBus, event_bus_for};
use crate::test_helpers;
use djinn_core::models::{Task, TaskStatus, TransitionAction};
use djinn_db::Database;
use djinn_db::EpicRepository;
use djinn_db::Error;
use djinn_db::{ActivityQuery, ReadyQuery, TaskRepository};
use tokio::sync::broadcast;
async fn make_epic(db: &Database, events: EventBus) -> djinn_core::models::Epic {
    EpicRepository::new(db.clone(), events)
        .create("Test Epic", "", "", "", "", None)
        .await
        .unwrap()
}

async fn open_task(repo: &TaskRepository, epic_id: &str) -> Task {
    let task = repo
        .create(epic_id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.update(
        &task.id,
        "T",
        "",
        "",
        0,
        "",
        "",
        r#"[{"description":"default","met":false}]"#,
    )
    .await
    .unwrap()
}

// ── Existing tests ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_get_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(
            &epic.id,
            "My Task",
            "",
            "",
            "task",
            0,
            "user@example.com",
            Some("open"),
        )
        .await
        .unwrap();
    assert_eq!(task.title, "My Task");
    assert_eq!(task.status, "open");
    assert_eq!(task.short_id.len(), 4);

    let fetched = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(fetched.title, "My Task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creating_task_reopens_closed_epic() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic_repo = EpicRepository::new(db.clone(), event_bus_for(&tx));
    let epic = epic_repo
        .create("Test Epic", "", "", "", "", None)
        .await
        .unwrap();
    epic_repo.close(&epic.id).await.unwrap();

    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));
    let _task = repo
        .create(&epic.id, "New Task", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();

    let reopened = epic_repo.get(&epic.id).await.unwrap().unwrap();
    assert_eq!(reopened.status, "open");
    assert!(reopened.closed_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_id_lookup() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let found = repo.get_by_short_id(&task.short_id).await.unwrap().unwrap();
    assert_eq!(found.id, task.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_emits_event() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let _ = rx.recv().await.unwrap(); // consume EpicCreated
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    repo.create(&epic.id, "Event Task", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "created");
    assert!(!envelope.from_sync);
    let t: Task = serde_json::from_value(envelope.payload["task"].clone()).unwrap();
    assert_eq!(t.title, "Event Task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_emits_event() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let _ = rx.recv().await.unwrap();
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "Old", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();

    let updated = repo
        .update(&task.id, "New", "desc", "details", 1, "task", "", "")
        .await
        .unwrap();
    assert_eq!(updated.title, "New");

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "updated");
    assert!(!envelope.from_sync);
    let t: Task = serde_json::from_value(envelope.payload["task"].clone()).unwrap();
    assert_eq!(t.id, task.id);
    assert_eq!(t.title, "New");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_emits_event_start() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let _ = rx.recv().await.unwrap();
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();
    repo.update(
        &task.id,
        "T",
        "",
        "",
        0,
        "",
        "",
        r#"[{"description":"default","met":false}]"#,
    )
    .await
    .unwrap();
    let _ = rx.recv().await.unwrap(); // drain TaskUpdated event from update

    repo.transition(&task.id, TransitionAction::Start, "", "system", None, None)
        .await
        .unwrap();

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "updated");
    assert!(!envelope.from_sync);
    let t: Task = serde_json::from_value(envelope.payload["task"].clone()).unwrap();
    assert_eq!(t.id, task.id);
    assert_eq!(t.status, "in_progress");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_emits_event_close_with_closed_at() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let _ = rx.recv().await.unwrap();
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();

    repo.transition(&task.id, TransitionAction::Close, "", "system", None, None)
        .await
        .unwrap();

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "updated");
    assert!(!envelope.from_sync);
    let t: Task = serde_json::from_value(envelope.payload["task"].clone()).unwrap();
    assert_eq!(t.id, task.id);
    assert_eq!(t.status, "closed");
    assert!(t.closed_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_emits_event_reopen() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let _ = rx.recv().await.unwrap();
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();

    repo.set_status(&task.id, "closed").await.unwrap();
    let _ = rx.recv().await.unwrap(); // task_updated event
    let _ = rx.recv().await.unwrap(); // activity_logged event from set_status

    repo.transition(
        &task.id,
        TransitionAction::Reopen,
        "system",
        "system",
        Some("reopening after verification"),
        None,
    )
    .await
    .unwrap();

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "updated");
    assert!(!envelope.from_sync);
    let t: Task = serde_json::from_value(envelope.payload["task"].clone()).unwrap();
    assert_eq!(t.id, task.id);
    assert_eq!(t.status, "open");
    assert!(t.closed_at.is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_status_transitions() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let updated = repo.set_status(&task.id, "in_progress").await.unwrap();
    assert_eq!(updated.status, "in_progress");

    let closed = repo.set_status(&task.id, "closed").await.unwrap();
    assert_eq!(closed.status, "closed");
    assert!(closed.closed_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_increments_counter() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.set_status(&task.id, "closed").await.unwrap();
    let reopened = repo.set_status(&task.id, "open").await.unwrap();
    assert_eq!(reopened.reopen_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocker_management() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "", Some("open"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocker_cycle_detection() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "", Some("open"))
        .await
        .unwrap();
    let t3 = repo
        .create(&epic.id, "T3", "", "", "task", 2, "", Some("open"))
        .await
        .unwrap();

    // t2 is blocked by t1; t3 is blocked by t2
    repo.add_blocker(&t2.id, &t1.id).await.unwrap();
    repo.add_blocker(&t3.id, &t2.id).await.unwrap();

    // Adding t1 blocked by t3 would create a cycle: t1 → t2 → t3 → t1
    let result = repo.add_blocker(&t1.id, &t3.id).await;
    assert!(result.is_err(), "expected cycle detection to reject this");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_blocked_by_unresolved_blocker() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let ac = r#"[{"description":"default","met":false}]"#;
    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.update(&t1.id, "T1", "", "", 0, "", "", ac)
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "", Some("open"))
        .await
        .unwrap();
    repo.update(&t2.id, "T2", "", "", 1, "", "", ac)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_ready_excludes_blocked_tasks() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let t1 = repo
        .create(&epic.id, "T1", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let t2 = repo
        .create(&epic.id, "T2", "", "", "task", 1, "", Some("open"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_log() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_by_epic() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    repo.create(&epic.id, "A", "", "", "task", 1, "", Some("open"))
        .await
        .unwrap();
    repo.create(&epic.id, "B", "", "", "feature", 0, "", Some("open"))
        .await
        .unwrap();

    let tasks = repo.list_by_epic(&epic.id).await.unwrap();
    assert_eq!(tasks.len(), 2);
    // Ordered by priority then created_at — B (priority 0) first.
    assert_eq!(tasks[0].title, "B");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_task_emits_event() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let _ = rx.recv().await.unwrap();
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "Del", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();

    repo.delete(&task.id).await.unwrap();
    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "deleted");
    assert_eq!(envelope.payload["id"].as_str().unwrap(), task.id);
}

// ── State machine tests ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_enum_roundtrips() {
    let statuses = [
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_happy_path() {
    let db = test_helpers::create_test_db();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_transition_returns_error() {
    let db = test_helpers::create_test_db();
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

    // Can't submit_verification from open (must be in_progress).
    let err = repo
        .transition(
            &task.id,
            TransitionAction::SubmitVerification,
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
    let db = test_helpers::create_test_db();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_review_reject_conflict_does_not_increment_reopen() {
    let db = test_helpers::create_test_db();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_close_from_any_state() {
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
async fn start_blocked_by_unresolved_blockers() {
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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
    let db = test_helpers::create_test_db();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_heals_stale_tasks() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

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

// ── SYNC-11: Terminal state protection tests ─────────────────────────────

fn make_peer_task(
    id: &str,
    project_id: &str,
    epic_id: &str,
    status: &str,
    updated_at: &str,
) -> Task {
    Task {
        id: id.to_string(),
        project_id: project_id.to_string(),
        short_id: format!("p{}", &id[..3]),
        epic_id: Some(epic_id.to_string()),
        title: "Peer Task".to_string(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: status.to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        verification_failure_count: 0,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: updated_at.to_string(),
        closed_at: if status == "closed" {
            Some(updated_at.to_string())
        } else {
            None
        },
        close_reason: if status == "closed" {
            Some("completed".to_string())
        } else {
            None
        },
        merge_commit_sha: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        unresolved_blocker_count: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_peer_closed_task_not_regressed() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create and close a task locally.
    let task = open_task(&repo, &epic.id).await;
    repo.set_status(&task.id, "closed").await.unwrap();

    // Peer sends the same task as in_progress with a LATER updated_at.
    let peer = make_peer_task(
        &task.id,
        &epic.project_id,
        &epic.id,
        "in_progress",
        "2099-01-01T00:00:00.000Z",
    );
    let changed = repo.upsert_peer(&peer).await.unwrap();
    assert!(!changed, "closed task should NOT be regressed by peer");

    let local = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(local.status, "closed", "task should remain closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_peer_closed_updated_by_peer_close() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create and close a task locally.
    let task = open_task(&repo, &epic.id).await;
    repo.set_status(&task.id, "closed").await.unwrap();

    // Peer sends the same task as closed with later updated_at and a new title.
    let mut peer = make_peer_task(
        &task.id,
        &epic.project_id,
        &epic.id,
        "closed",
        "2099-01-01T00:00:00.000Z",
    );
    peer.title = "Updated Title From Peer".to_string();
    let changed = repo.upsert_peer(&peer).await.unwrap();
    assert!(
        changed,
        "closed→closed update with newer timestamp should succeed"
    );

    let local = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(local.title, "Updated Title From Peer");
    assert_eq!(local.status, "closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_peer_non_terminal_lww_works() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    // Create an open task.
    let task = open_task(&repo, &epic.id).await;

    // Peer sends it as in_progress with a later updated_at.
    let peer = make_peer_task(
        &task.id,
        &epic.project_id,
        &epic.id,
        "in_progress",
        "2099-01-01T00:00:00.000Z",
    );
    let changed = repo.upsert_peer(&peer).await.unwrap();
    assert!(changed, "non-terminal LWW should update");

    let local = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(local.status, "in_progress");
}

// ── SYNC-12: Closed task eviction tests ──────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_for_export_includes_open_tasks() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let _t1 = open_task(&repo, &epic.id).await;
    let _t2 = open_task(&repo, &epic.id).await;

    let exported = repo.list_for_export(None).await.unwrap();
    assert_eq!(exported.len(), 2, "all open tasks should be exported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_for_export_includes_recently_closed() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    repo.set_status(&task.id, "closed").await.unwrap();

    // Task was just closed — should be included.
    let exported = repo.list_for_export(None).await.unwrap();
    assert!(
        exported.iter().any(|t| t.id == task.id),
        "recently closed task should be in export"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_for_export_excludes_old_closed() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    repo.set_status(&task.id, "closed").await.unwrap();

    // Backdate closed_at to 2 hours ago.
    sqlx::query("UPDATE tasks SET closed_at = datetime('now', '-2 hours') WHERE id = ?1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let exported = repo.list_for_export(None).await.unwrap();
    assert!(
        !exported.iter().any(|t| t.id == task.id),
        "task closed >1 hour ago should be evicted from export"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn increment_continuation_count_emits_task_updated_event() {
    let db = test_helpers::create_test_db();
    let (tx, mut rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let _ = rx.recv().await.unwrap(); // consume EpicCreated
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // consume TaskCreated

    repo.increment_continuation_count(&task.id).await.unwrap();

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "task");
    assert_eq!(envelope.action, "updated");
    assert!(!envelope.from_sync);
    let t: Task = serde_json::from_value(envelope.payload["task"].clone()).unwrap();
    assert_eq!(t.id, task.id);
    assert_eq!(t.continuation_count, task.continuation_count + 1);
}

/// Proves that `update_blockers_atomic` eliminates the race window:
/// the task is never visible to `list_ready` during an atomic swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocker_swap_atomic_no_race_window() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let blocker_a = repo
        .create(&epic.id, "Blocker-A", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let blocker_b = repo
        .create(&epic.id, "Blocker-B", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let task = repo
        .create(
            &epic.id,
            "Blocked-Task",
            "",
            "",
            "task",
            1,
            "",
            Some("open"),
        )
        .await
        .unwrap();

    // task is blocked by blocker_a.
    repo.add_blocker(&task.id, &blocker_a.id).await.unwrap();

    // Atomic swap: remove blocker_a and add blocker_b in one transaction.
    repo.update_blockers_atomic(&task.id, &[blocker_b.id.clone()], &[blocker_a.id.clone()])
        .await
        .unwrap();

    // Task should still be blocked (by blocker_b now).
    let ready = repo.list_ready(ReadyQuery::default()).await.unwrap();
    assert!(
        !ready.iter().any(|t| t.id == task.id),
        "task should be blocked by blocker_b after atomic swap"
    );

    // Verify the blocker was actually swapped.
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(blockers.len(), 1);
    assert_eq!(blockers[0].task_id, blocker_b.id);
}

/// Concurrent variant: races atomic blocker swaps against `claim` to confirm
/// the dispatcher can never grab a task mid-swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocker_swap_atomic_no_race_concurrent() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = Arc::new(TaskRepository::new(db, event_bus_for(&tx)));

    let blocker_a = repo
        .create(&epic.id, "Blocker-A", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let blocker_b = repo
        .create(&epic.id, "Blocker-B", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let task = repo
        .create(
            &epic.id,
            "Blocked-Task",
            "",
            "",
            "task",
            1,
            "",
            Some("open"),
        )
        .await
        .unwrap();

    // Start with task blocked by blocker_a.
    repo.add_blocker(&task.id, &blocker_a.id).await.unwrap();

    let claimed = Arc::new(AtomicBool::new(false));
    let iterations = 200;

    // Claimer: repeatedly tries to claim the task.
    let claimer = {
        let repo = Arc::clone(&repo);
        let task_id = task.id.clone();
        let claimed = Arc::clone(&claimed);
        tokio::spawn(async move {
            for _ in 0..iterations * 2 {
                if let Ok(Some(t)) = repo.claim(ReadyQuery::default(), "test", "system").await {
                    if t.id == task_id {
                        claimed.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
    };

    // Swapper: repeatedly swaps blockers atomically.
    let swapper = {
        let repo = Arc::clone(&repo);
        let task_id = task.id.clone();
        let blocker_a_id = blocker_a.id.clone();
        let blocker_b_id = blocker_b.id.clone();
        tokio::spawn(async move {
            for i in 0..iterations {
                if i % 2 == 0 {
                    let _ = repo
                        .update_blockers_atomic(
                            &task_id,
                            &[blocker_b_id.clone()],
                            &[blocker_a_id.clone()],
                        )
                        .await;
                } else {
                    let _ = repo
                        .update_blockers_atomic(
                            &task_id,
                            &[blocker_a_id.clone()],
                            &[blocker_b_id.clone()],
                        )
                        .await;
                }
                tokio::task::yield_now().await;
            }
        })
    };

    swapper.await.unwrap();
    claimer.await.unwrap();

    assert!(
        !claimed.load(Ordering::SeqCst),
        "claim() must never grab a task during an atomic blocker swap"
    );
}
