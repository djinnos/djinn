//! Operator-control parity for the durable refinement dispatch path.
//!
//! `dispatch_next_refinement_phase` honours three operator gates — the
//! administrative dispatch pause, `proposals.build_frozen`
//! (`proposal_stop_build --freeze`), and a terminal proposal status — but it is
//! unreachable for a run with a `run_id`: `drive_one_refinement` early-returns
//! for durable runs because the leased intent ledger is their only dispatch
//! path. These tests lock the gates onto that ledger, and lock the two
//! properties that make gating safe:
//!
//!   * a gated tick creates NO role task and enqueues NOTHING, and
//!   * the durable intent is not consumed, leased away, terminalized, or
//!     parked — the very next ungated tick dispatches the same intent.

use djinn_core::{
    events::{DjinnEventEnvelope, EventBus},
    refinement_liveness::{RefinementIntentState, RefinementRunState},
};
use djinn_db::{
    DispatchPauseTarget, LoadRefinementRunSnapshotRequest, ProposalRepository, TaskRepository,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::refinement_cap_tests::{self, TEST_MODEL};
use super::refinement_durable_dispatch_tests::{admit_run, only_intent};
use crate::actor::CoordinatorActor;

const HEARTBEAT_GRACE_MILLIS: i64 = 60_000;

type ObservedDispatches = tokio::sync::mpsc::UnboundedReceiver<(String, String)>;

/// A pool whose runner only records `(task_id, model_id)`, so a dispatch that
/// reaches a slot is observable without running a real agent session.
fn spawn_observing_pool(
    db: &djinn_db::Database,
) -> (djinn_slot::SlotPoolHandle, ObservedDispatches) {
    let cancel = CancellationToken::new();
    let (observed_tx, observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let pool = djinn_slot::SlotPoolHandle::spawn_with_factory(
        crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
        cancel,
        djinn_slot::SlotPoolConfig {
            models: vec![djinn_slot::ModelSlotConfig {
                model_id: TEST_MODEL.to_owned(),
                max_slots: 1,
                roles: ["advocate", "adversary", "judge", "worker"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            }],
            role_priorities: HashMap::new(),
        },
        Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
            let observed_tx = observed_tx.clone();
            let runner: djinn_slot::TestLifecycleRunner =
                Arc::new(move |task_id, _, model_id, _, _, _, _| {
                    let observed_tx = observed_tx.clone();
                    Box::pin(async move {
                        observed_tx
                            .send((task_id, model_id))
                            .expect("record gated-path dispatch");
                        Ok(())
                    })
                });
            djinn_slot::SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    (pool, observed_rx)
}

/// One durable run plus everything needed to assert against it.
struct DurableGateCase {
    db: djinn_db::Database,
    repo: ProposalRepository,
    fixture: refinement_cap_tests::RefinementFixture,
    run_id: String,
    generation: i32,
    intent_id: String,
}

/// Seed a durable run plus the disposable run-keyed projection a live tribunal
/// has.
async fn durable_gate_fixture(
    db: &djinn_db::Database,
    events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
    idempotency_key: &str,
) -> (CoordinatorActor, ObservedDispatches, DurableGateCase) {
    let fixture = refinement_cap_tests::seed_refinement_fixture(db).await;
    let (pool, observed) = spawn_observing_pool(db);
    let mut actor = refinement_cap_tests::build_refinement_actor(db, events_tx, pool);
    let repo = ProposalRepository::new(db.clone(), EventBus::noop());
    let (run_id, generation) = admit_run(&repo, &fixture.proposal_id, idempotency_key).await;
    let intent_id = only_intent(&repo, &run_id, generation).await;
    let projection =
        super::super::refinement::RefinementLoopState::new(fixture.proposal_id.clone(), 0)
            .with_run_identity(run_id.clone(), generation)
            .with_attributed_user(Some(fixture.user_id.clone()));
    actor.active_refinements.insert(run_id.clone(), projection);
    (
        actor,
        observed,
        DurableGateCase {
            db: db.clone(),
            repo,
            fixture,
            run_id,
            generation,
            intent_id,
        },
    )
}

impl DurableGateCase {
    async fn dispatchable_intents(&self) -> Vec<djinn_db::RefinementPendingIntent> {
        self.repo
            .load_dispatchable_refinement_intents(&self.run_id, self.generation)
            .await
            .expect("read dispatchable intents")
    }

    async fn role_task_id(&self) -> Option<String> {
        TaskRepository::new(self.db.clone(), EventBus::noop())
            .find_by_refinement_intent_id(&self.intent_id)
            .await
            .expect("read role task for intent")
            .map(|task| task.id)
    }

    /// A gated tick must create no role task, enqueue nothing, and leave the
    /// durable intent exactly as dispatchable as it was.
    async fn assert_gated_and_work_preserved(
        &self,
        actor: &CoordinatorActor,
        observed: &mut ObservedDispatches,
        gate: &str,
    ) {
        assert!(
            self.role_task_id().await.is_none(),
            "{gate}: a gated durable tick must not create a tribunal role task"
        );
        assert!(
            observed.try_recv().is_err(),
            "{gate}: a gated durable tick must not enqueue anything into the pool"
        );
        assert!(
            actor.refinement_sessions.is_empty(),
            "{gate}: a gated durable tick must not manufacture an outcome projection"
        );

        // The intent is not consumed: still dispatchable, still `pending`, still
        // unleased, so the next ungated tick claims it normally.
        let intents = self.dispatchable_intents().await;
        assert_eq!(
            intents.len(),
            1,
            "{gate}: the gated intent must survive the tick"
        );
        assert_eq!(
            intents[0].state,
            RefinementIntentState::Pending,
            "{gate}: a gated tick must leave the intent pending, never claimed or terminal"
        );
        assert_eq!(
            intents[0].intent_id, self.intent_id,
            "{gate}: the surviving intent must be the same intent"
        );
        assert!(
            intents[0].claimed_by.is_none(),
            "{gate}: a gated tick must not hold a lease on the intent"
        );

        // The run itself is untouched: not terminalized, not parked.
        let snapshot = self
            .repo
            .load_refinement_run_snapshot(LoadRefinementRunSnapshotRequest {
                run_id: self.run_id.clone(),
                heartbeat_grace_millis: HEARTBEAT_GRACE_MILLIS,
            })
            .await
            .expect("read exact run after gated tick")
            .expect("gated run still exists");
        assert_eq!(
            snapshot.snapshot.run.state,
            RefinementRunState::Active,
            "{gate}: gating must not terminalize the run"
        );
        assert_eq!(
            snapshot.snapshot.run.terminal_reason, None,
            "{gate}: gating must not record a stop reason"
        );
        assert!(
            snapshot.snapshot.park.is_none(),
            "{gate}: gating must not park the run"
        );
    }

    /// Once the gate clears, the same preserved intent dispatches.
    async fn assert_dispatches_after_gate_clears(
        &self,
        actor: &mut CoordinatorActor,
        observed: &mut ObservedDispatches,
        gate: &str,
    ) {
        actor.drive_active_refinements().await;

        let task_id = self.role_task_id().await.unwrap_or_else(|| {
            panic!("{gate}: the preserved intent must dispatch once the gate clears")
        });
        let (dispatched_task_id, dispatched_model_id) =
            tokio::time::timeout(Duration::from_secs(5), observed.recv())
                .await
                .unwrap_or_else(|_| panic!("{gate}: ungated dispatch must reach the pool"))
                .unwrap_or_else(|| panic!("{gate}: ungated dispatch observation"));
        assert_eq!(dispatched_task_id, task_id);
        assert_eq!(dispatched_model_id, TEST_MODEL);
        assert_eq!(
            self.dispatchable_intents().await[0].state,
            RefinementIntentState::Materialized,
            "{gate}: the ungated tick must materialize the same intent"
        );
    }
}

// ── Gate 1: administrative dispatch pause ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_pause_stops_durable_refinement_and_preserves_the_intent() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let (mut actor, mut observed, case) =
        durable_gate_fixture(&db, &events_tx, "durable-gate-pause").await;

    djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&events_tx))
        .pause(
            DispatchPauseTarget::Global,
            djinn_core::models::DispatchPause {
                paused_by: "durable-gate-test".to_owned(),
                paused_at: ::time::OffsetDateTime::now_utc()
                    .format(&::time::format_description::well_known::Rfc3339)
                    .expect("format pause timestamp"),
                reason: "operator halted dispatch".to_owned(),
                expires_at: None,
            },
        )
        .await
        .expect("pause dispatch globally");

    actor.drive_active_refinements().await;
    case.assert_gated_and_work_preserved(&actor, &mut observed, "dispatch_pause")
        .await;

    djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&events_tx))
        .resume(DispatchPauseTarget::Global)
        .await
        .expect("resume dispatch");
    case.assert_dispatches_after_gate_clears(&mut actor, &mut observed, "dispatch_pause")
        .await;
}

// ── Gate 2: proposal build freeze (`proposal_stop_build --freeze`) ──────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_freeze_stops_durable_refinement_and_preserves_the_intent() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let (mut actor, mut observed, case) =
        durable_gate_fixture(&db, &events_tx, "durable-gate-freeze").await;

    case.repo
        .set_frozen(&case.fixture.proposal_id, true)
        .await
        .expect("freeze the proposal build");

    actor.drive_active_refinements().await;
    case.assert_gated_and_work_preserved(&actor, &mut observed, "build_frozen")
        .await;

    case.repo
        .set_frozen(&case.fixture.proposal_id, false)
        .await
        .expect("unfreeze the proposal build");
    case.assert_dispatches_after_gate_clears(&mut actor, &mut observed, "build_frozen")
        .await;
}

// ── Gate 3: terminal proposal status ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_proposal_status_stops_durable_refinement_and_preserves_the_intent() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let (mut actor, mut observed, case) =
        durable_gate_fixture(&db, &events_tx, "durable-gate-terminal").await;

    case.repo
        .set_status(&case.fixture.proposal_id, "rejected")
        .await
        .expect("move the proposal to a terminal status");

    actor.drive_active_refinements().await;
    case.assert_gated_and_work_preserved(&actor, &mut observed, "terminal_status")
        .await;

    // Restoring a non-terminal status is not a supported operator flow; it is
    // only proof that the terminal gate SKIPPED rather than consumed the intent.
    case.repo
        .set_status(&case.fixture.proposal_id, "building")
        .await
        .expect("restore a non-terminal status");
    case.assert_dispatches_after_gate_clears(&mut actor, &mut observed, "terminal_status")
        .await;
}

// ── The gate must not block outcome recovery for a role that already ran ────

/// A gate stops NEW dispatch; it must not orphan a role session that is already
/// running. The recovery arm (materialized intent + an observed agent session)
/// still re-registers the in-flight projection while the gate holds, so the
/// round's outcome is processed instead of being stranded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gate_does_not_strand_a_role_session_that_already_started() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
    let (mut actor, mut observed, case) =
        durable_gate_fixture(&db, &events_tx, "durable-gate-recovery").await;

    // Dispatch the round while ungated, then drop the disposable projection as a
    // coordinator restart would.
    actor.drive_active_refinements().await;
    let task_id = case
        .role_task_id()
        .await
        .expect("ungated tick dispatches the round");
    let _ = tokio::time::timeout(Duration::from_secs(5), observed.recv())
        .await
        .expect("ungated dispatch reaches the pool");
    actor.refinement_sessions.clear();

    // The role's agent session exists, so the round is genuinely in flight.
    djinn_db::SessionRepository::new(db.clone(), EventBus::noop())
        .create(djinn_db::CreateSessionParams {
            project_id: &case.fixture.project_id,
            task_id: Some(&task_id),
            model: TEST_MODEL,
            agent_type: "adversary",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("materialize the running role session");

    case.repo
        .set_frozen(&case.fixture.proposal_id, true)
        .await
        .expect("freeze the proposal build mid-round");

    actor.drive_active_refinements().await;

    assert!(
        actor.refinement_sessions.contains_key(&case.run_id),
        "a gate must not strand an already-running role: the in-flight projection \
         has to be re-registered so its outcome is still processed"
    );
    assert!(
        observed.try_recv().is_err(),
        "the already-running role must not be re-enqueued while the gate holds"
    );
    assert_eq!(
        case.dispatchable_intents().await[0].state,
        RefinementIntentState::Materialized,
        "the gated tick must leave the materialized intent and its task intact"
    );
}
