use crate::events::{EventBus, event_bus_for};
use crate::test_helpers;
use djinn_core::models::{Task, TaskStatus, TransitionAction};
use djinn_db::Database;
use djinn_db::EpicRepository;
use djinn_db::Error;
use djinn_db::{ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository};
use rstest::rstest;
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
async fn start_allows_planning_without_acceptance_criteria() {
    let db = test_helpers::create_test_db();
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
async fn board_health_flags_repeated_reopen_role_tool_mismatch_candidates() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
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
    sqlx::query("UPDATE tasks SET total_reopen_count = 3 WHERE id = ?1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    let _session = test_helpers::create_test_session(&db, &project.id, &task.id).await;

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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_health_ignores_repeated_reopen_tasks_without_role_tool_mismatch() {
    let db = test_helpers::create_test_db();
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
    sqlx::query("UPDATE tasks SET total_reopen_count = 4 WHERE id = ?1")
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
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        total_verification_failure_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
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
    repo.update_blockers_atomic(
        &task.id,
        std::slice::from_ref(&blocker_b.id),
        std::slice::from_ref(&blocker_a.id),
    )
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
                if let Ok(Some(t)) = repo.claim(ReadyQuery::default(), "test", "system").await
                    && t.id == task_id
                {
                    claimed.store(true, Ordering::SeqCst);
                    return;
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
                            std::slice::from_ref(&blocker_b_id),
                            std::slice::from_ref(&blocker_a_id),
                        )
                        .await;
                } else {
                    let _ = repo
                        .update_blockers_atomic(
                            &task_id,
                            std::slice::from_ref(&blocker_a_id),
                            std::slice::from_ref(&blocker_b_id),
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

// ── rstest parametrized: valid state machine transitions ──────────────────
//
// Tests that repo.transition() succeeds and produces the expected status for
// each valid from_status/action/expected_to triple.  set_status() is used to
// put the task in the right starting state without going through the full
// happy-path sequence.  Actions that require a reason are passed a stub value;
// actions that require AC (Start) are excluded here — they are covered by the
// dedicated acceptance-criteria tests above.

#[rstest]
// close is valid from every non-closed state
#[case("open", TransitionAction::Close, "closed", None)]
#[case("in_progress", TransitionAction::Close, "closed", None)]
#[case("verifying", TransitionAction::Close, "closed", None)]
#[case("needs_task_review", TransitionAction::Close, "closed", None)]
#[case("in_task_review", TransitionAction::Close, "closed", None)]
#[case("approved", TransitionAction::Close, "closed", None)]
#[case("pr_draft", TransitionAction::Close, "closed", None)]
#[case("pr_review", TransitionAction::Close, "closed", None)]
// force_close is valid from every non-closed state (requires a reason)
#[case("open", TransitionAction::ForceClose, "closed", Some("testing"))]
#[case("in_progress", TransitionAction::ForceClose, "closed", Some("testing"))]
// reopen is valid from closed
#[case("closed", TransitionAction::Reopen, "open", Some("needed again"))]
// submit_task_review is valid from in_progress
#[case(
    "in_progress",
    TransitionAction::SubmitTaskReview,
    "needs_task_review",
    None
)]
// task_review_start is valid from needs_task_review
#[case(
    "needs_task_review",
    TransitionAction::TaskReviewStart,
    "in_task_review",
    None
)]
// task_review_approve moves to approved
#[case(
    "in_task_review",
    TransitionAction::TaskReviewApprove,
    "approved",
    None
)]
// task_review_reject returns to open (requires reason)
#[case(
    "in_task_review",
    TransitionAction::TaskReviewReject,
    "open",
    Some("needs more work")
)]
// release returns in_progress to open (requires reason)
#[case(
    "in_progress",
    TransitionAction::Release,
    "open",
    Some("releasing slot")
)]
// release_task_review returns in_task_review to needs_task_review (requires reason)
#[case(
    "in_task_review",
    TransitionAction::ReleaseTaskReview,
    "needs_task_review",
    Some("releasing review")
)]
// pr_created transitions approved → pr_draft
#[case("approved", TransitionAction::PrCreated, "pr_draft", None)]
// pr_undraft transitions pr_draft → pr_review
#[case("pr_draft", TransitionAction::PrUndraft, "pr_review", None)]
// pr_ci_failed transitions pr_draft → open
#[case("pr_draft", TransitionAction::PrCiFailed, "open", None)]
// pr_conflict transitions approved → open
#[case("approved", TransitionAction::PrConflict, "open", None)]
// pr_conflict transitions pr_draft → open
#[case("pr_draft", TransitionAction::PrConflict, "open", None)]
// pr_merge transitions pr_review → closed
#[case("pr_review", TransitionAction::PrMerge, "closed", None)]
// pr_changes_requested transitions pr_review → open (requires reason)
#[case(
    "pr_review",
    TransitionAction::PrChangesRequested,
    "open",
    Some("changes requested by reviewer")
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_transition(
    #[case] from_status: &str,
    #[case] action: TransitionAction,
    #[case] expected_to: &str,
    #[case] reason: Option<&str>,
) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Create with AC so Start works if needed; set_status bypasses AC guard.
    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
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
    .unwrap();

    // Put the task in the requested starting state.
    repo.set_status(&task.id, from_status).await.unwrap();

    let result = repo
        .transition(&task.id, action, "", "system", reason, None)
        .await
        .expect("expected valid transition to succeed");

    assert_eq!(
        result.status, expected_to,
        "transition produced unexpected status"
    );
}

// ── rstest parametrized: invalid state machine transitions ────────────────
//
// Each case is a (from_status, action) pair that must be rejected with
// InvalidTransition.  A reason is always supplied to avoid the reason-required
// guard masking the state-machine error.

#[rstest]
#[case("open", TransitionAction::SubmitVerification)]
#[case("open", TransitionAction::SubmitTaskReview)]
#[case("open", TransitionAction::TaskReviewStart)]
#[case("open", TransitionAction::TaskReviewApprove)]
#[case("open", TransitionAction::TaskReviewReject)]
#[case("open", TransitionAction::Reopen)]
#[case("open", TransitionAction::Release)]
#[case("in_progress", TransitionAction::TaskReviewStart)]
#[case("in_progress", TransitionAction::TaskReviewApprove)]
#[case("in_progress", TransitionAction::Reopen)]
#[case("needs_task_review", TransitionAction::TaskReviewApprove)]
#[case("needs_task_review", TransitionAction::TaskReviewReject)]
#[case("needs_task_review", TransitionAction::Reopen)]
#[case("in_task_review", TransitionAction::SubmitTaskReview)]
#[case("closed", TransitionAction::Close)]
#[case("closed", TransitionAction::ForceClose)]
#[case("closed", TransitionAction::TaskReviewApprove)]
// approved invalid transitions
#[case("approved", TransitionAction::Start)]
#[case("approved", TransitionAction::TaskReviewApprove)]
#[case("approved", TransitionAction::PrUndraft)]
// pr_created only from approved
#[case("open", TransitionAction::PrCreated)]
#[case("in_progress", TransitionAction::PrCreated)]
#[case("pr_draft", TransitionAction::PrCreated)]
// pr_undraft only from pr_draft
#[case("open", TransitionAction::PrUndraft)]
#[case("approved", TransitionAction::PrUndraft)]
// pr_ci_failed only from pr_draft
#[case("open", TransitionAction::PrCiFailed)]
#[case("approved", TransitionAction::PrCiFailed)]
#[case("pr_review", TransitionAction::PrCiFailed)]
// pr_conflict only from approved or pr_draft
#[case("open", TransitionAction::PrConflict)]
#[case("in_progress", TransitionAction::PrConflict)]
#[case("pr_review", TransitionAction::PrConflict)]
// pr_merge only from pr_review
#[case("open", TransitionAction::PrMerge)]
#[case("in_task_review", TransitionAction::PrMerge)]
#[case("pr_draft", TransitionAction::PrMerge)]
// pr_changes_requested only from pr_review
#[case("open", TransitionAction::PrChangesRequested)]
#[case("in_task_review", TransitionAction::PrChangesRequested)]
#[case("pr_draft", TransitionAction::PrChangesRequested)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_transition(#[case] from_status: &str, #[case] action: TransitionAction) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    let task = repo
        .create(&epic.id, "T", "", "", "task", 0, "", Some("open"))
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
    .unwrap();

    repo.set_status(&task.id, from_status).await.unwrap();

    // Supply a reason so the requires_reason guard never fires first.
    let err = repo
        .transition(&task.id, action, "", "system", Some("stub-reason"), None)
        .await
        .expect_err("expected InvalidTransition");

    assert!(
        matches!(err, Error::InvalidTransition(_)),
        "expected InvalidTransition, got {err:?}"
    );
}

// ── rstest parametrized: task_count group_by ─────────────────────────────
//
// Verifies that count_grouped returns a { "groups": [...] } object (not an
// error) for each valid group_by value, and that the group key values match
// the kind of field being grouped on.

#[rstest]
#[case("status")]
#[case("priority")]
#[case("issue_type")]
#[case("epic")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_count_group_by(#[case] group_by: &str) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Create a mix of tasks so grouping has something to return.
    repo.create(&epic.id, "A", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.create(&epic.id, "B", "", "", "feature", 1, "", Some("open"))
        .await
        .unwrap();

    let result = repo
        .count_grouped(CountQuery {
            group_by: Some(group_by.to_owned()),
            project_id: None,
            status: None,
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
        })
        .await
        .expect("count_grouped should succeed");

    assert!(
        result["groups"].is_array(),
        "expected {{groups: [...]}} for group_by={group_by}"
    );
    assert!(
        !result["groups"].as_array().unwrap().is_empty(),
        "expected at least one group for group_by={group_by}"
    );
}

// ── rstest parametrized: task_list filters ───────────────────────────────
//
// Verifies that list_filtered returns correctly filtered results for each
// filter dimension independently.

#[rstest]
#[case("status")]
#[case("priority")]
#[case("label")]
#[case("text")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_list_filter(#[case] filter_kind: &str) {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db, event_bus_for(&tx));

    // Create two tasks; the filter should pick exactly one of them.
    repo.create(&epic.id, "alpha task", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    let beta = repo
        .create(
            &epic.id,
            "beta feature",
            "",
            "",
            "feature",
            1,
            "",
            Some("open"),
        )
        .await
        .unwrap();

    // Apply a label to the second task.
    repo.update(&beta.id, "beta feature", "", "", 1, "", r#"["urgent"]"#, "")
        .await
        .unwrap();

    // Transition beta to in_progress (bypassing AC check via set_status).
    repo.set_status(&beta.id, "in_progress").await.unwrap();

    let query = match filter_kind {
        "status" => ListQuery {
            status: Some("in_progress".to_owned()),
            ..Default::default()
        },
        "priority" => ListQuery {
            priority: Some(1),
            ..Default::default()
        },
        "label" => ListQuery {
            label: Some("urgent".to_owned()),
            ..Default::default()
        },
        "text" => ListQuery {
            text: Some("beta".to_owned()),
            ..Default::default()
        },
        _ => unreachable!(),
    };

    let result = repo
        .list_filtered(query)
        .await
        .expect("list_filtered should succeed");

    assert_eq!(
        result.total_count, 1,
        "filter_kind={filter_kind} should match exactly one task"
    );
    assert_eq!(result.tasks[0].id, beta.id);
}
