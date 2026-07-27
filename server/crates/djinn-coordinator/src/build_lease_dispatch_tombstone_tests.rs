//! The board-wide dispatch wedge of 2026-07-27, end to end.
//!
//! Symptom: zero active sessions, six dispatchable tasks, and 78
//! `build admission denied; leaving task queued` lines in five minutes — every
//! one of them at `cap=3` with `occupancy` 0 or 1 and a request weighing 1. No
//! capacity rule can deny at that occupancy. The board never recovered on its
//! own and would not have recovered from a restart either.
//!
//! # The mechanism, which is a closed loop
//!
//! A task's dispatch slot is keyed `task_dispatch/{task_id}:{generation}`.
//!
//! 1. The pool is full, so admission denies and leaves a QUEUED lease row under
//!    that key. It returns `Denied` *before* the journal reservation.
//! 2. Nothing claims the position before its queue deadline, so the row goes
//!    `terminal/deadline_expired` — or the task is closed and reopened and the
//!    row goes `terminal/abandoned`, or the reclaimer proves its object absent
//!    and it goes `terminal/reclaimed_absent`.
//! 3. `queue()` had no state filter, so it replayed that terminal row forever.
//!    The replay became `LeaseWaitTimeout` / `LeaseUnavailable`, which the slot
//!    authority converted into a denial.
//! 4. Because the denial returns before the journal write, no journal row is
//!    ever created, so `resolve_dispatch_generation` returns the SAME
//!    generation next tick — so the key never changes, so the same tombstone is
//!    read again. Nothing in the loop can advance.
//!
//! Emptying the pool does not help. That is what makes these tests falsifiable:
//! each one drives the pool to occupancy ZERO and then demands the task
//! actually dispatch.
//!
//! # What stays green if the fix does nothing
//!
//! Nothing. Every assertion below is on a side effect — the weighted
//! `build_leases` occupancy the cap is really compared against, and an
//! `admit_task_run` that must return `Permitted`. Reverting the
//! `spent_dispatch_attempt` branch in `BuildLeaseRepository::queue` fails both
//! tests on the `permitted(...)` call, which is the outage itself, not a
//! counter.

use djinn_db::{
    AdmissionDomain, BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseState,
    BuildLeaseTerminalReason, Database,
};
use djinn_k8s::{WarmAdmission, WarmAdmissionPermit, WarmAdmissionTransition};

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode, DenialCause,
};
use crate::build_admission_capacity_support::{CapacityHarness, controller_with_capacity_over};

const EPOCH: &str = "coordinator:tombstone-epoch";

/// Well past any dispatch queue deadline this composition can mint. Passed to
/// the same `expire_deadlines` the coordinator tick calls, so the row is
/// retired by the production sweep rather than by a hand-written UPDATE.
const AFTER_THE_DEADLINE: &str = "2099-01-01T00:00:00Z";

async fn harness(cap: i64) -> CapacityHarness {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    controller_with_capacity_over(&db, BuildAdmissionMode::Enforce, cap, EPOCH).await
}

fn lease_key(task_id: &str, generation: i64) -> BuildLeaseKey {
    BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskDispatch,
        consumer_id: format!("{task_id}:{generation}"),
    }
}

async fn admit(controller: &BuildAdmissionController, task_id: &str) -> BuildAdmissionDecision {
    controller
        .admit_task_run(
            Some("worker"),
            AdmissionDomain::TaskObservation,
            task_id.to_owned(),
            0,
            format!("task-run-{task_id}-0"),
        )
        .await
        .expect("admission must be reachable")
}

async fn permitted(controller: &BuildAdmissionController, task_id: &str) -> WarmAdmissionPermit {
    match admit(controller, task_id).await {
        BuildAdmissionDecision::Permitted { permit, .. } => permit,
        other => panic!("{task_id} must be permitted over an EMPTY pool, got {other:?}"),
    }
}

/// Read the durable terminal reason on a dispatch key, asserting the row is
/// terminal. This is the tombstone, produced by production code only.
async fn tombstone(harness: &CapacityHarness, task_id: &str) -> String {
    let row = harness
        .leases
        .get(&lease_key(task_id, 0))
        .await
        .unwrap()
        .expect("the denied attempt must have left a durable dispatch row");
    assert_eq!(
        row.state,
        BuildLeaseState::Terminal,
        "the attempt must be spent for this to be the wedge under test"
    );
    row.terminal_reason
        .expect("a terminal row names its reason")
}

/// The five-task branch: `deadline_expired`.
///
/// A task denied behind a full pool leaves a queue position; the position
/// outlives its deadline and is swept; and from then on the task is refused
/// forever — including, crucially, when the pool is completely empty.
#[tokio::test]
async fn a_deadline_expired_dispatch_attempt_does_not_wedge_its_task_forever() {
    let harness = harness(1).await;

    // Fill the single slot with real warm capacity, so the denial that follows
    // is a genuine capacity denial and not an artefact of the fixture.
    let held = harness
        .hold_warm_lease("holder")
        .await
        .expect("the warm holder must take the only slot");
    assert_eq!(harness.occupancy().await, 1);

    assert_eq!(
        admit(&harness.controller, "victim").await,
        BuildAdmissionDecision::Denied {
            occupancy: Some(1),
            cap: 1,
            cause: DenialCause::AtCapacity,
        },
        "a full pool denies, and says so with the occupancy it actually read"
    );

    // The coordinator's own deadline sweep, run at a time past the position's
    // 30-minute queue deadline.
    harness
        .leases
        .expire_deadlines(AFTER_THE_DEADLINE)
        .await
        .unwrap();
    assert_eq!(
        tombstone(&harness, "victim").await,
        BuildLeaseTerminalReason::DeadlineExpired.as_str()
    );

    // The pool empties completely. Nothing is occupying anything.
    harness.release_warm_lease(held).await;
    assert_eq!(
        harness.occupancy().await,
        0,
        "the precondition that makes any further denial indefensible"
    );

    // The side effect, first: the task must actually dispatch.
    let _permit = permitted(&harness.controller, "victim").await;
    assert_eq!(
        harness.occupancy().await,
        1,
        "and it must dispatch by TAKING the free slot, not by being waved through"
    );

    // The generation must NOT have been consumed by the denials.
    //
    // A `Denied` outcome returns before the journal reservation, so no journal
    // row is written and `resolve_dispatch_generation` keeps returning the same
    // number. That is correct — a denied attempt reserved nothing and must not
    // burn a generation — but it is exactly what made the wedge self-sustaining
    // once the key under that generation was tombstoned. With the tombstone
    // retired, the generation the denials preserved is the one the success
    // takes.
    assert!(
        harness
            .journal
            .generation_state(AdmissionDomain::TaskObservation, "victim", 0)
            .await
            .unwrap()
            .is_some(),
        "the successful admission must land on generation 0 — the generation \
         every preceding denial deliberately left unconsumed"
    );

    // Corroboration: the durable row is a live occupying attempt again, not the
    // retired one replayed.
    let row = harness
        .leases
        .get(&lease_key("victim", 0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, BuildLeaseState::Launching);
    assert!(
        row.fencing_token.is_some(),
        "a fresh attempt carries a fresh fencing token, so the retired \
         attempt's token is fenced out"
    );
}

/// The `vjjb` branch: `abandoned`.
///
/// This one reached the slot authority's `_ =>` arm, so it was reported as
/// `Unavailable` and printed with a FABRICATED `occupancy=0` — a denial
/// asserting a capacity figure it never consulted. Both halves are asserted:
/// the task dispatches, and no denial invents a number.
#[tokio::test]
async fn an_abandoned_dispatch_attempt_does_not_wedge_its_task_forever() {
    let harness = harness(1).await;
    let held = harness
        .hold_warm_lease("holder")
        .await
        .expect("the warm holder must take the only slot");

    assert!(matches!(
        admit(&harness.controller, "victim").await,
        BuildAdmissionDecision::Denied {
            cause: DenialCause::AtCapacity,
            ..
        }
    ));

    // The production path a closed/reopened task takes: its queued dispatch
    // position is surrendered rather than left to be granted to nobody.
    assert_eq!(
        harness
            .leases
            .abandon_queued_dispatch("victim")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        tombstone(&harness, "victim").await,
        BuildLeaseTerminalReason::Abandoned.as_str()
    );

    harness.release_warm_lease(held).await;
    assert_eq!(harness.occupancy().await, 0);

    let _permit = permitted(&harness.controller, "victim").await;
    assert_eq!(harness.occupancy().await, 1);
}

/// A denial must never again print an occupancy it did not read.
///
/// Enforced structurally rather than by scraping a log line: only
/// [`DenialCause::AtCapacity`] may carry `Some(occupancy)`, and it is the only
/// cause the cap can produce.
#[tokio::test]
async fn only_a_capacity_denial_reports_an_occupancy() {
    let harness = harness(1).await;
    let held = harness.hold_warm_lease("holder").await.unwrap();
    let denial = admit(&harness.controller, "victim").await;
    harness.release_warm_lease(held).await;

    match denial {
        BuildAdmissionDecision::Denied {
            occupancy,
            cap,
            cause,
        } => {
            assert_eq!(cause, DenialCause::AtCapacity);
            assert_eq!(occupancy, Some(1));
            assert_eq!(cap, 1);
            assert!(
                occupancy.is_some_and(|occupancy| occupancy.saturating_add(1) > cap),
                "a capacity denial must be arithmetically capable of justifying \
                 itself: this is the assertion the outage's log line failed"
            );
        }
        other => panic!("expected a capacity denial, got {other:?}"),
    }
}

/// A retired dispatch attempt must not resurrect a slot it never handed back.
///
/// The retirement is a DELETE, so it is worth proving it cannot be aimed at an
/// occupying row: the fresh attempt has to queue behind live capacity like any
/// other newcomer.
#[tokio::test]
async fn retiring_a_spent_attempt_never_frees_capacity_it_did_not_own() {
    let harness = harness(1).await;
    let holder = permitted(&harness.controller, "holder").await;
    assert_eq!(harness.occupancy().await, 1);

    // A second admission for the SAME task replays the live attempt: it is this
    // attempt's own capacity, held, not a second slot.
    assert!(matches!(
        admit(&harness.controller, "holder").await,
        BuildAdmissionDecision::Permitted { .. }
    ));
    assert_eq!(
        harness.occupancy().await,
        1,
        "an occupying attempt is replayed, never retired and re-bought"
    );

    // And a different task still contends normally.
    assert!(matches!(
        admit(&harness.controller, "contender").await,
        BuildAdmissionDecision::Denied {
            cause: DenialCause::AtCapacity,
            ..
        }
    ));
    assert_eq!(harness.occupancy().await, 1);

    // The holder hands its slot back through the production release path, and
    // only then does the contender get it.
    harness
        .controller
        .transition(
            &holder,
            WarmAdmissionTransition::DefinitiveFailure {
                diagnostic: "slot-pool rejected before task-run creation".to_owned(),
            },
        )
        .await
        .expect("release the holder");

    // The contender's queue position -- taken during the denial above -- is
    // drained onto the freed slot, and its next admission adopts that grant.
    // One slot in, one slot out: retiring nothing, double-granting nothing.
    let _permit = permitted(&harness.controller, "contender").await;
    assert_eq!(
        harness.occupancy().await,
        1,
        "the cap of 1 is still exactly one slot: the holder's capacity moved to \
         the contender, it was not duplicated"
    );
}
