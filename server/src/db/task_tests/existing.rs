use super::*;

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
