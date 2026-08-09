use crate::evidence_dispatch_recovery::{
    EvidenceDispatchTestOutcome, evidence_dispatch_test_count, set_evidence_dispatch_test_script,
};
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::{
    Database, TaskRepository,
    test_support::{
        EvidenceDispatchRecoveryFixtureForTest, EvidenceDispatchRecoverySnapshotForTest,
        close_evidence_dispatch_task_for_test, evidence_dispatch_recovery_snapshot_for_test,
        materialize_evidence_dispatch_recovery_fixture_for_test,
    },
};

#[derive(Debug)]
struct Fixture {
    db: Database,
    allocation: EvidenceDispatchRecoveryFixtureForTest,
}
async fn fixture(is_retry: bool) -> Fixture {
    let db = crate::test_helpers::create_test_db();
    let allocation = materialize_evidence_dispatch_recovery_fixture_for_test(&db, is_retry).await;
    Fixture { db, allocation }
}
async fn snapshot(f: &Fixture) -> EvidenceDispatchRecoverySnapshotForTest {
    evidence_dispatch_recovery_snapshot_for_test(&f.db, &f.allocation).await
}
async fn assert_state(
    f: &Fixture,
    stable: &EvidenceDispatchRecoverySnapshotForTest,
    lifecycle: &str,
    errors: i64,
) {
    let current = snapshot(f).await;
    assert_eq!(current.finding_id, stable.finding_id);
    assert_eq!(current.proposal_id, stable.proposal_id);
    assert_eq!(current.demand_hash, stable.demand_hash);
    assert_eq!(current.attempt_id, stable.attempt_id);
    assert_eq!(current.attempt_sequence, stable.attempt_sequence);
    assert_eq!(current.spike_task_id, stable.spike_task_id);
    assert_eq!(current.legacy_link, stable.legacy_link);
    assert_eq!(current.finding_slot_count, 1);
    assert_eq!(current.attempt_count, 1);
    assert_eq!(current.lifecycle, lifecycle);
    assert_eq!(current.dispatch_error_count, errors);
}
async fn coordinator_fault_injection_case(is_retry: bool) {
    let f = fixture(is_retry).await;
    let (events_tx, _) = tokio::sync::broadcast::channel(32);
    let (mut actor, cancel) =
        crate::test_helpers::make_coordinator_actor_cancellable(&f.db, &events_tx);
    let task = TaskRepository::new(f.db.clone(), djinn_core::events::EventBus::noop())
        .get(&f.allocation.spike_task_id)
        .await
        .unwrap()
        .unwrap();
    let live = DjinnEventEnvelope::task_created(&task, false);
    let stable = snapshot(&f).await;
    set_evidence_dispatch_test_script(
        &f.allocation.spike_task_id,
        [
            EvidenceDispatchTestOutcome::EnqueueFailed,
            EvidenceDispatchTestOutcome::EnqueueFailed,
            EvidenceDispatchTestOutcome::Accepted,
            EvidenceDispatchTestOutcome::AlreadyActive,
        ],
        true,
    );
    actor.handle_event(live.clone()).await;
    assert_state(&f, &stable, "demanded", 1).await;
    actor.handle_event(live).await;
    assert_state(&f, &stable, "demanded", 2).await;
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "demanded", 2).await;
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "spike_active", 2).await;
    assert_eq!(evidence_dispatch_test_count(&f.allocation.spike_task_id), 4);
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "spike_active", 2).await;
    close_evidence_dispatch_task_for_test(&f.db, &f.allocation.spike_task_id).await;
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "spike_active", 2).await;
    assert_eq!(snapshot(&f).await.task_status, "closed");
    assert_eq!(evidence_dispatch_test_count(&f.allocation.spike_task_id), 4);
    cancel.cancel();
}
async fn terminal_allocation_is_never_dispatched(is_retry: bool) {
    let f = fixture(is_retry).await;
    let stable = snapshot(&f).await;
    close_evidence_dispatch_task_for_test(&f.db, &f.allocation.spike_task_id).await;
    set_evidence_dispatch_test_script(
        &f.allocation.spike_task_id,
        [EvidenceDispatchTestOutcome::Accepted],
        false,
    );
    let (events_tx, _) = tokio::sync::broadcast::channel(32);
    let (mut actor, cancel) =
        crate::test_helpers::make_coordinator_actor_cancellable(&f.db, &events_tx);
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "demanded", 0).await;
    actor.redrive_demanded_evidence_dispatches().await;
    assert_state(&f, &stable, "demanded", 0).await;
    assert_eq!(snapshot(&f).await.task_status, "closed");
    assert_eq!(evidence_dispatch_test_count(&f.allocation.spike_task_id), 0);
    cancel.cancel();
}
#[tokio::test]
async fn initial_demand_redrive_recovers_exact_accepted_attempt() {
    coordinator_fault_injection_case(false).await;
    terminal_allocation_is_never_dispatched(false).await;
}
#[tokio::test]
async fn retry_redrive_recovers_exact_accepted_attempt() {
    coordinator_fault_injection_case(true).await;
    terminal_allocation_is_never_dispatched(true).await;
}
