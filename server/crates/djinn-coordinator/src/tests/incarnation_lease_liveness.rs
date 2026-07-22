//! Regression coverage for the 2026-07-22 self-reap incident.
//!
//! The single live coordinator renewed its incarnation lease only on the 15-min
//! stale sweep, while the orphan reaper reads a lease as "expired" after 5 min.
//! So the live owner's lease was always stale at reap time and its own orphaned
//! pending dispatches were mis-stamped `interrupted` / `environmental_owner_expired`
//! — a strike-exempt outcome that let a genuinely-failing task loop forever.
//!
//! The fix renews the lease every 60s from a dedicated task, so a live owner
//! reads truthfully live and its genuine orphans classify as `crashed`
//! (strike-counting). These tests pin the reaper's owner-liveness contract that
//! the renewal cadence exists to satisfy: a freshly-renewed owner reaps as
//! `crashed`; a stale owner reaps as environmental `interrupted`.

use super::*;
use djinn_db::{CoordinatorIncarnationRepository, CreateTaskAttemptParams, TaskAttemptRepository};

/// Threshold used for both the age gate and the lease-liveness window in these
/// tests. Small so the tests stay fast while leaving a comfortable margin over
/// the post-seed sleep.
const REAP_THRESHOLD_SECS: i64 = 2;

/// Seed a `pending` attempt owned by `owner`, with no live task_run/session, so
/// it is an orphan-reaper candidate once older than the age gate.
async fn seed_pending_attempt(db: &Database, task_id: &str, owner: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("{task_id}:lead:{id}");
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id,
            role: "lead",
            dispatch_key: &dispatch_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: Some(owner),
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    id
}

async fn reaped_attempt(db: &Database, task_id: &str, attempt_id: &str) -> (String, String) {
    let attempts = TaskAttemptRepository::new(db.clone())
        .list_for_task(task_id)
        .await
        .unwrap();
    let attempt = attempts
        .into_iter()
        .find(|a| a.id == attempt_id)
        .expect("seeded attempt must still exist");
    let failure_class = attempt
        .summary_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| v["failure_class"].as_str().map(str::to_owned))
        .unwrap_or_default();
    (attempt.outcome, failure_class)
}

/// A live owner (lease renewed within the liveness window) must reap its
/// orphaned pending attempt as `crashed` / `orphaned_pending_attempt` — a
/// strike-counting outcome — never as strike-exempt environmental interruption.
/// This is what the 60s renewal cadence guarantees in production.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_owner_orphan_reaps_as_crashed_not_environmental() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let (task, _path) = create_simple_task(&db, &tx, "task", "live owner orphan").await;

    let owner = uuid::Uuid::now_v7().to_string();
    CoordinatorIncarnationRepository::new(db.clone())
        .register(&owner)
        .await
        .unwrap();
    let attempt_id = seed_pending_attempt(&db, &task.id, &owner).await;

    // Age the attempt past the gate, then renew so the owner reads live — the
    // production invariant the dedicated renewal task upholds.
    tokio::time::sleep(std::time::Duration::from_millis(
        (REAP_THRESHOLD_SECS as u64) * 1000 + 600,
    ))
    .await;
    CoordinatorIncarnationRepository::new(db.clone())
        .renew(&owner)
        .await
        .unwrap();

    crate::health::reap_orphaned_pending_attempts_with_threshold(
        &db,
        REAP_THRESHOLD_SECS,
        "periodic",
    )
    .await;

    let (outcome, failure_class) = reaped_attempt(&db, &task.id, &attempt_id).await;
    assert_eq!(
        outcome, "crashed",
        "a live owner's orphan must reap as strike-counting `crashed`"
    );
    assert_eq!(
        failure_class, "orphaned_pending_attempt",
        "a live owner's orphan must NOT be classified environmental_owner_expired"
    );
}

/// Contrast: an owner whose lease has aged past the window (the pre-fix 15-min
/// starvation) still reaps as environmental `interrupted`. This confirms the
/// classification hinges on lease freshness — the exact signal the renewal
/// cadence controls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_owner_orphan_reaps_as_environmental_interrupted() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(16);
    let (task, _path) = create_simple_task(&db, &tx, "task", "stale owner orphan").await;

    let owner = uuid::Uuid::now_v7().to_string();
    CoordinatorIncarnationRepository::new(db.clone())
        .register(&owner)
        .await
        .unwrap();
    let attempt_id = seed_pending_attempt(&db, &task.id, &owner).await;

    // Never renew: the lease ages past the window alongside the attempt.
    tokio::time::sleep(std::time::Duration::from_millis(
        (REAP_THRESHOLD_SECS as u64) * 1000 + 600,
    ))
    .await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(
        &db,
        REAP_THRESHOLD_SECS,
        "periodic",
    )
    .await;

    let (outcome, failure_class) = reaped_attempt(&db, &task.id, &attempt_id).await;
    assert_eq!(
        outcome, "interrupted",
        "a genuinely-expired owner's orphan reaps as environmental interrupted"
    );
    assert_eq!(failure_class, "environmental_owner_expired");
}
