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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactional_peer_upsert_round_trips_distinct_refinement_correlations() {
    let db = create_test_db();
    let (events, _rx) = broadcast::channel(64);
    let epic = make_epic(&db, event_bus_for(&events)).await;
    let first_id = uuid::Uuid::now_v7().to_string();
    let second_id = uuid::Uuid::now_v7().to_string();
    let proposal_id = uuid::Uuid::now_v7().to_string();
    let first_run_id = uuid::Uuid::now_v7().to_string();
    let second_run_id = uuid::Uuid::now_v7().to_string();
    let first_intent_id = uuid::Uuid::now_v7().to_string();
    let second_intent_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq) VALUES ($1, $2, 'Test proposal', '', 'markdown', '[]'::jsonb, 'draft', 1)")
        .bind(&proposal_id)
        .bind(format!("p{}", &proposal_id[..8]))
        .execute(db.pool())
        .await
        .unwrap();
    for (run_id, generation, intent_id) in [
        (&first_run_id, 1, &first_intent_id),
        (&second_run_id, 2, &second_intent_id),
    ] {
        sqlx::query("INSERT INTO refinement_runs (id, proposal_id, generation, idempotency_key, state, terminal_at, stop_tag) VALUES ($1, $2, $3, $4, 'terminal', '2026-01-01T00:00:00.000Z', 'operator_stop')")
            .bind(run_id)
            .bind(&proposal_id)
            .bind(generation)
            .bind(format!("test-{generation}"))
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO refinement_dispatch_intents (id, run_id, round, phase, role, idempotency_key) VALUES ($1, $2, 1, 'adversary_attack', 'adversary', $3)")
            .bind(intent_id)
            .bind(run_id)
            .bind(format!("intent-{generation}"))
            .execute(db.pool())
            .await
            .unwrap();
    }
    let mut first = make_peer_task(
        &first_id,
        &epic.project_id,
        &epic.id,
        "open",
        "2099-01-01T00:00:00.000Z",
    );
    let mut second = make_peer_task(
        &second_id,
        &epic.project_id,
        &epic.id,
        "open",
        "2099-01-01T00:00:00.000Z",
    );
    // UUIDv7 values produced in one millisecond share a prefix; make the peer
    // short IDs distinct so this test reaches the correlation projection.
    first.short_id = "peer-one".into();
    second.short_id = "peer-two".into();
    first.refinement_run_id = Some(first_run_id.clone());
    first.refinement_intent_id = Some(first_intent_id.clone());
    first.refinement_generation = Some(1);
    first.refinement_round = Some(1);
    first.refinement_phase = Some("adversary_attack".into());
    first.refinement_role = Some("adversary".into());
    second.refinement_run_id = Some(second_run_id.clone());
    second.refinement_intent_id = Some(second_intent_id.clone());
    second.refinement_generation = Some(1);
    second.refinement_round = Some(1);
    second.refinement_phase = Some("adversary_attack".into());
    second.refinement_role = Some("adversary".into());

    let mut tx = db.pool().begin().await.unwrap();
    assert!(
        TaskRepository::upsert_peer_in_tx(&mut tx, &first)
            .await
            .unwrap()
    );
    assert!(
        TaskRepository::upsert_peer_in_tx(&mut tx, &second)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();

    let repo = TaskRepository::new(db.clone(), event_bus_for(&events));
    let reloaded_first = repo.get(&first_id).await.unwrap().unwrap();
    let reloaded_second = repo.get(&second_id).await.unwrap().unwrap();
    assert_eq!(
        reloaded_first.refinement_run_id.as_deref(),
        Some(first_run_id.as_str())
    );
    assert_eq!(
        reloaded_first.refinement_intent_id.as_deref(),
        Some(first_intent_id.as_str())
    );
    assert_eq!(
        reloaded_second.refinement_run_id.as_deref(),
        Some(second_run_id.as_str())
    );
    assert_eq!(
        reloaded_second.refinement_intent_id.as_deref(),
        Some(second_intent_id.as_str())
    );
    assert_ne!(
        reloaded_first.refinement_correlation().unwrap(),
        reloaded_second.refinement_correlation().unwrap()
    );
}
