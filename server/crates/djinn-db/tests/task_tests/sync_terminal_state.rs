use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_peer_closed_task_not_regressed() {
    let db = create_test_db();
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
    let db = create_test_db();
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
    let db = create_test_db();
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
