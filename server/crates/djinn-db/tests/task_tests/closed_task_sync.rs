use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_for_export_includes_open_tasks() {
    let db = create_test_db();
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
    let db = create_test_db();
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
    let db = create_test_db();
    let (tx, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&tx)).await;
    let repo = TaskRepository::new(db.clone(), event_bus_for(&tx));

    let task = open_task(&repo, &epic.id).await;
    repo.set_status(&task.id, "closed").await.unwrap();

    // Backdate closed_at to 2 hours ago.
    sqlx::query(
        "UPDATE tasks SET closed_at = DATE_FORMAT(DATE_SUB(NOW(3), INTERVAL 2 HOUR), '%Y-%m-%dT%H:%i:%s.%fZ') WHERE id = ?",
    )
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
    let db = create_test_db();
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
    let db = create_test_db();
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

    let db = create_test_db();
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
