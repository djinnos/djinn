//! Exact-run terminalization failure regression matrix.

use std::time::Instant as StdInstant;

use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    refinement_liveness::{RefinementRunState, RefinementStopReason},
};
use djinn_db::{
    AdmitRefinementRunRequest, LoadRefinementRunSnapshotRequest, ProposalRepository,
    RefinementAdmissionOutcome, RefinementAdmissionSource,
};
use djinn_slot::SlotPoolHandle;
use tokio::sync::mpsc;

use super::*;
use crate::refinement_dispatch::refinement_cap_tests;

async fn exact_run_fixture() -> (
    crate::actor::CoordinatorActor,
    djinn_db::Database,
    refinement_cap_tests::RefinementFixture,
    String,
    i32,
) {
    let db = crate::test_helpers::create_test_db();
    let fixture = refinement_cap_tests::seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
    let (pool_tx, _pool_rx) = mpsc::channel(1);
    let mut actor = refinement_cap_tests::build_refinement_actor(
        &db,
        &events_tx,
        SlotPoolHandle::from_raw_sender(pool_tx),
    );
    let admission = ProposalRepository::new(db.clone(), EventBus::noop())
        .admit_refinement_run(AdmitRefinementRunRequest {
            proposal_id: fixture.proposal_id.clone(),
            idempotency_key: uuid::Uuid::now_v7().to_string(),
            source: RefinementAdmissionSource::Demand {
                demand_id: uuid::Uuid::now_v7().to_string(),
            },
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("admit exact durable termination fixture");
    let (run_id, generation) = match admission {
        RefinementAdmissionOutcome::Admitted {
            run_id, generation, ..
        }
        | RefinementAdmissionOutcome::Existing {
            run_id, generation, ..
        } => (run_id, generation),
    };
    assert!(
        !run_id.is_empty(),
        "fixture requires an exact durable run ID"
    );
    assert!(generation > 0, "fixture requires a durable generation");
    actor.active_refinements.insert(
        run_id.clone(),
        RefinementLoopState::new(fixture.proposal_id.clone(), 1)
            .with_run_identity(run_id.clone(), generation),
    );
    actor.refinement_sessions.insert(
        run_id.clone(),
        RefinementSession {
            run_id: run_id.clone(),
            generation,
            task_id: format!("terminal-failure-fixture-{run_id}"),
            phase: RefinementPhase::AdversaryAttack,
            dispatched_at: StdInstant::now(),
            session_started_at: None,
            model_id: refinement_cap_tests::TEST_MODEL.to_owned(),
        },
    );
    (actor, db, fixture, run_id, generation)
}

async fn audit_snapshot(
    db: &djinn_db::Database,
    proposal_id: &str,
    run_id: &str,
) -> djinn_db::test_support::RefinementRunReadOnlySnapshotForTest {
    djinn_db::test_support::refinement_run_read_only_snapshot_for_test(db, proposal_id, run_id)
        .await
}

#[tokio::test]
async fn terminal_cas_miss_retains_exact_run_actor_projection_and_session() {
    let (mut actor, db, fixture, run_id, durable_generation) = exact_run_fixture().await;
    let stale_generation = durable_generation + 1;
    actor
        .active_refinements
        .get_mut(&run_id)
        .expect("exact run projection")
        .generation = stale_generation;
    actor
        .refinement_sessions
        .get_mut(&run_id)
        .expect("exact run session")
        .generation = stale_generation;
    let before = audit_snapshot(&db, &fixture.proposal_id, &run_id).await;
    let projection_before = format!("{:#?}", actor.active_refinements[&run_id]);
    let session_before = format!("{:#?}", actor.refinement_sessions[&run_id]);

    assert!(
        !actor
            .terminate_refinement(&run_id, RefinementStopReason::RoundCap)
            .await,
        "stale exact generation must not publish terminalization"
    );

    let durable = ProposalRepository::new(db.clone(), EventBus::noop())
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: run_id.clone(),
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("load exact durable run")
        .expect("admitted exact durable run remains");
    let after = audit_snapshot(&db, &fixture.proposal_id, &run_id).await;
    assert_eq!(durable.generation, durable_generation);
    assert_eq!(durable.snapshot.run.state, RefinementRunState::Active);
    assert_eq!(
        before, after,
        "CAS miss must leave durable run and lifecycle unchanged"
    );
    assert!(
        after
            .lifecycle_rows
            .iter()
            .all(|row| row.event_kind != "refinement_stop")
    );
    assert_eq!(
        format!("{:#?}", actor.active_refinements[&run_id]),
        projection_before
    );
    assert_eq!(
        format!("{:#?}", actor.refinement_sessions[&run_id]),
        session_before
    );
}

#[tokio::test]
#[tracing_test::traced_test]
async fn terminal_repository_error_retains_exact_run_actor_projection_and_session() {
    let (mut actor, db, fixture, run_id, generation) = exact_run_fixture().await;
    let before = audit_snapshot(&db, &fixture.proposal_id, &run_id).await;
    let projection_before = format!("{:#?}", actor.active_refinements[&run_id]);
    let session_before = format!("{:#?}", actor.refinement_sessions[&run_id]);
    djinn_db::test_support::reject_refinement_terminal_audit_for_test(&db).await;

    assert!(
        !actor
            .terminate_refinement(&run_id, RefinementStopReason::RoundCap)
            .await,
        "repository failure must not publish terminalization"
    );

    let durable = ProposalRepository::new(db.clone(), EventBus::noop())
        .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
            run_id: run_id.clone(),
            heartbeat_grace_millis: 60_000,
        })
        .await
        .expect("load exact run after repository failure")
        .expect("repository error leaves the exact durable run retryable");
    let after = audit_snapshot(&db, &fixture.proposal_id, &run_id).await;
    assert_eq!(durable.generation, generation);
    assert_eq!(durable.snapshot.run.state, RefinementRunState::Active);
    assert_eq!(
        before, after,
        "failed transaction must not leave a false terminal snapshot"
    );
    assert!(
        after
            .lifecycle_rows
            .iter()
            .all(|row| row.event_kind != "refinement_stop")
    );
    assert_eq!(
        format!("{:#?}", actor.active_refinements[&run_id]),
        projection_before
    );
    assert_eq!(
        format!("{:#?}", actor.refinement_sessions[&run_id]),
        session_before
    );
    assert!(logs_contain(
        "failed to terminalize durable refinement run; retaining retryable projection"
    ));
}
