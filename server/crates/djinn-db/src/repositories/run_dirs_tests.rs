//! Fresh-DB tests for the run-dir ledger. Each test uses the template-cloned
//! ephemeral Postgres harness via [`Database::open_in_memory`].

use super::*;

const VOLUME: &str = "node-vol-a";

fn key(pod: &str) -> RunDirKey {
    RunDirKey {
        volume_id: VOLUME.into(),
        pod_uid: pod.into(),
    }
}

fn reserve_input(pod: &str) -> ReserveRunDirInput {
    ReserveRunDirInput {
        key: key(pod),
        task_run_id: Some(format!("run-{pod}")),
        project_id: Some("proj-1".into()),
        base_fingerprint: Some("fp-1".into()),
        reserved_bytes: 4_096,
        quota_id: Some(format!("quota-{pod}")),
    }
}

#[tokio::test]
async fn reserve_inserts_reserved_row() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let row = repo.reserve(&reserve_input("p1")).await.unwrap();
    assert_eq!(row.state, RunDirState::Reserved);
    assert_eq!(row.generation, 0);
    assert_eq!(row.reserved_bytes, 4_096);
    assert_eq!(row.quota_id.as_deref(), Some("quota-p1"));
    assert!(row.final_path.is_none());
}

#[tokio::test]
async fn reserve_is_idempotent_while_reserved() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let first = repo.reserve(&reserve_input("p1")).await.unwrap();
    let second = repo.reserve(&reserve_input("p1")).await.unwrap();
    assert_eq!(first.generation, second.generation);
    assert_eq!(second.state, RunDirState::Reserved);
}

#[tokio::test]
async fn full_lifecycle_reserve_to_absent() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let k = key("p1");
    let reserved = repo.reserve(&reserve_input("p1")).await.unwrap();
    assert_eq!(reserved.generation, 0);

    let seeding = repo
        .mark_seeding(&k, 0, "/cache/runs/.seed-p1-0")
        .await
        .unwrap();
    assert_eq!(seeding.state, RunDirState::Seeding);
    assert_eq!(seeding.temp_path.as_deref(), Some("/cache/runs/.seed-p1-0"));

    let ready = repo
        .mark_ready_active(&k, 0, "/cache/runs/p1", 9_999)
        .await
        .unwrap();
    assert_eq!(ready.state, RunDirState::ReadyActive);
    assert_eq!(ready.final_path.as_deref(), Some("/cache/runs/p1"));
    assert_eq!(ready.measured_bytes, 9_999);
    assert!(ready.last_lease_at.is_some());

    let idle = repo.mark_ready_idle(&k, 0).await.unwrap();
    assert_eq!(idle.state, RunDirState::ReadyIdle);

    let active_again = repo.touch_lease(&k, 0).await.unwrap();
    assert_eq!(active_again.state, RunDirState::ReadyActive);

    let reclaimable = repo.mark_reclaimable(&k, 0).await.unwrap();
    assert_eq!(reclaimable.state, RunDirState::Reclaimable);

    let reclaiming = repo.mark_reclaiming(&k, 0).await.unwrap();
    assert_eq!(reclaiming.state, RunDirState::Reclaiming);
    // Reclaiming bumps the generation to fence a racing acquire.
    assert_eq!(reclaiming.generation, 1);

    let absent = repo.mark_absent_after_reclaim(&k, 1).await.unwrap();
    assert_eq!(absent.state, RunDirState::Absent);
    assert_eq!(absent.reserved_bytes, 0);
    assert!(absent.quota_id.is_none());
}

#[tokio::test]
async fn stale_generation_transition_fails_closed() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let k = key("p1");
    repo.reserve(&reserve_input("p1")).await.unwrap();
    // Expected generation 7 does not match durable 0.
    assert!(matches!(
        repo.mark_seeding(&k, 7, "/tmp/x").await,
        Err(DbError::InvalidTransition(_))
    ));
}

#[tokio::test]
async fn gc_reclaiming_fences_a_racing_acquire() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let k = key("p1");
    repo.reserve(&reserve_input("p1")).await.unwrap();
    repo.mark_seeding(&k, 0, "/tmp/seed").await.unwrap();
    repo.mark_ready_active(&k, 0, "/cache/runs/p1", 1)
        .await
        .unwrap();
    repo.mark_ready_idle(&k, 0).await.unwrap();

    // GC wins: ready_idle -> reclaiming bumps generation 0 -> 1.
    let reclaiming = repo.mark_reclaiming(&k, 0).await.unwrap();
    assert_eq!(reclaiming.generation, 1);

    // A late acquire holding the pre-GC generation cannot re-activate.
    assert!(matches!(
        repo.touch_lease(&k, 0).await,
        Err(DbError::InvalidTransition(_))
    ));
}

#[tokio::test]
async fn release_reservation_from_reserved_and_seeding() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);

    let k1 = key("p1");
    repo.reserve(&reserve_input("p1")).await.unwrap();
    let released = repo.release_reservation(&k1, 0).await.unwrap();
    assert_eq!(released.state, RunDirState::Absent);
    assert_eq!(released.reserved_bytes, 0);

    let k2 = key("p2");
    repo.reserve(&reserve_input("p2")).await.unwrap();
    repo.mark_seeding(&k2, 0, "/tmp/seed-p2").await.unwrap();
    let released = repo.release_reservation(&k2, 0).await.unwrap();
    assert_eq!(released.state, RunDirState::Absent);
}

#[tokio::test]
async fn reserving_an_absent_row_bumps_generation() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let k = key("p1");
    repo.reserve(&reserve_input("p1")).await.unwrap();
    repo.release_reservation(&k, 0).await.unwrap();
    // Re-reserving the same identity after absent advances the generation so a
    // stale prior-generation callback cannot match.
    let reserved = repo.reserve(&reserve_input("p1")).await.unwrap();
    assert_eq!(reserved.state, RunDirState::Reserved);
    assert_eq!(reserved.generation, 1);
}

#[tokio::test]
async fn upsert_reconciled_is_idempotent_and_quarantines() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let input = ReconciledRunDirInput {
        key: key("unknown-pod"),
        task_run_id: None,
        project_id: None,
        base_fingerprint: None,
        state: RunDirState::QuarantinedUnowned,
        measured_bytes: 12_345,
        final_path: Some("/cache/runs/unknown-pod".into()),
    };
    let first = repo.upsert_reconciled(&input).await.unwrap();
    assert_eq!(first.state, RunDirState::QuarantinedUnowned);
    assert_eq!(first.measured_bytes, 12_345);

    // Re-running reconciliation never overwrites an existing row.
    let mut mutated = input.clone();
    mutated.measured_bytes = 999;
    let second = repo.upsert_reconciled(&mutated).await.unwrap();
    assert_eq!(second.measured_bytes, 12_345);
}

#[tokio::test]
async fn upsert_reconciled_ready_active_sets_lease() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let input = ReconciledRunDirInput {
        key: key("live-pod"),
        task_run_id: Some("run-live".into()),
        project_id: Some("proj-1".into()),
        base_fingerprint: Some("fp-1".into()),
        state: RunDirState::ReadyActive,
        measured_bytes: 5_000,
        final_path: Some("/cache/runs/live-pod".into()),
    };
    let row = repo.upsert_reconciled(&input).await.unwrap();
    assert_eq!(row.state, RunDirState::ReadyActive);
    assert!(row.last_lease_at.is_some());
}

#[tokio::test]
async fn volume_state_totals_aggregate_by_state() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    // One reserved (4096 reserved bytes), one reconciled ready_active.
    repo.reserve(&reserve_input("p1")).await.unwrap();
    repo.upsert_reconciled(&ReconciledRunDirInput {
        key: key("p2"),
        task_run_id: Some("run-p2".into()),
        project_id: Some("proj-1".into()),
        base_fingerprint: Some("fp-1".into()),
        state: RunDirState::ReadyActive,
        measured_bytes: 7_000,
        final_path: Some("/cache/runs/p2".into()),
    })
    .await
    .unwrap();

    let totals = repo.volume_state_totals(VOLUME).await.unwrap();
    let reserved = totals
        .iter()
        .find(|t| t.state == RunDirState::Reserved)
        .unwrap();
    assert_eq!(reserved.count, 1);
    assert_eq!(reserved.reserved_bytes, 4_096);
    let ready = totals
        .iter()
        .find(|t| t.state == RunDirState::ReadyActive)
        .unwrap();
    assert_eq!(ready.count, 1);
    assert_eq!(ready.measured_bytes, 7_000);
}

#[tokio::test]
async fn latest_measured_bytes_projection() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    assert_eq!(
        repo.latest_measured_bytes("proj-1", "fp-1").await.unwrap(),
        None
    );
    let k = key("p1");
    repo.reserve(&reserve_input("p1")).await.unwrap();
    repo.mark_seeding(&k, 0, "/tmp/seed").await.unwrap();
    repo.mark_ready_active(&k, 0, "/cache/runs/p1", 8_192)
        .await
        .unwrap();
    assert_eq!(
        repo.latest_measured_bytes("proj-1", "fp-1").await.unwrap(),
        Some(8_192)
    );
}

#[tokio::test]
async fn illegal_transition_is_rejected() {
    let db = Database::open_in_memory().unwrap();
    let repo = RunDirRepository::new(db);
    let k = key("p1");
    repo.reserve(&reserve_input("p1")).await.unwrap();
    // Cannot go straight from reserved to ready_active.
    assert!(matches!(
        repo.mark_ready_active(&k, 0, "/cache/runs/p1", 1).await,
        Err(DbError::InvalidTransition(_))
    ));
}
