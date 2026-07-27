//! The production dispatch outage of 2026-07-25, end to end.
//!
//! Symptom: `build admission denied; leaving task queued  occupancy=3 cap=3`
//! for EVERY task for ~40 minutes, with zero task-run Pods and zero Jobs in the
//! namespace. Three occupying `task_dispatch` rows in `build_leases` survived a
//! full `djinn-server` rollout restart, that restart's journal recovery, the
//! startup Kubernetes inventory reconciliation (which reported
//! `stale_rows=2 reclaimed=2` — a *different* ledger), and the deletion of every
//! `Complete` task-run Job.
//!
//! Nothing here writes a lease or journal row by hand. The leak is produced by
//! the production composition: a real admission through
//! [`BuildAdmissionController`] over the real [`BuildLeaseService`], a real
//! predecessor-epoch recovery, and a real
//! [`BuildAdmissionReconciler`] pass — after which the v0 lifecycle ledger is
//! empty and the v1 capacity ledger is still full. Producing that shape is as
//! much the behaviour under test as the reclamation is.
//!
//! # What stays green if reclamation does nothing
//!
//! Nothing that matters. Every assertion below is on a side effect:
//! [`CapacityHarness::occupancy`] (the weighted `build_leases` sum the cap is
//! actually compared against) and an `admit_task_run` that returns
//! `Permitted`. Neutralizing `BuildLeaseReclaimer::ownerless_proof` for the
//! dispatch branch fails `dispatch_leak_wedges_every_task_until_it_is_reclaimed`
//! on the occupancy assertion and again on the admit.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use djinn_db::{
    AdmissionDomain, AdmissionJournalRepository, AdmissionState, BuildLeaseConsumerKind,
    BuildLeaseKey, BuildLeaseState, Database,
};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WarmAdmission, WarmAdmissionPermit, WarmAdmissionTransition,
    WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode, DenialCause,
};
use crate::build_admission_capacity_support::{CapacityHarness, controller_with_capacity_over};
use crate::build_admission_inventory::BuildAdmissionReconciler;
use crate::build_lease_reclaim::{BuildLeaseReclaimer, dispatch_identity};
use crate::build_slot_authority::BuildLeaseDispatchAuthority;

const PREDECESSOR: &str = "coordinator-a:predecessor-epoch";
const SUCCESSOR: &str = "coordinator-b:successor-epoch";

/// A namespace with no Jobs at all: exactly what `kubectl get jobs -n djinn`
/// answered while occupancy read 3.
struct EmptyNamespace;

#[async_trait]
impl WorkloadInventory for EmptyNamespace {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Ok(Vec::new())
    }
    async fn get_uid(&self, _: WorkloadObjectKind, _: &str, _: &str) -> UidGetResult {
        UidGetResult::NotFound
    }
    async fn presence(&self, _: WorkloadObjectKind, _: &str) -> ObjectPresence {
        ObjectPresence::Absent
    }
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
        other => panic!("{task_id} must be permitted, got {other:?}"),
    }
}

/// Dispatch one task-run exactly as `begin_task_run_build_admission` /
/// `finish_task_run_build_admission` do: acquire the layer-1 slot, record the
/// create, and leave the generation in `create_unknown` because the slot pool
/// returns no Kubernetes UID.
async fn dispatch_task_run(harness: &CapacityHarness, task_id: &str) {
    let permit = permitted(&harness.controller, task_id).await;
    harness
        .controller
        .transition(&permit, WarmAdmissionTransition::CreateStarted)
        .await
        .expect("record the create");
    harness
        .controller
        .transition(
            &permit,
            WarmAdmissionTransition::CreateUnknown {
                diagnostic: "slot-pool accepted create without object UID".to_owned(),
            },
        )
        .await
        .expect("record the ambiguous create outcome");
}

/// The replacement process: a fresh server epoch over the same durable ledgers
/// AND the same surviving capacity authority, running the two startup
/// reconciliations in production order.
///
/// Sharing the authority is the point. A successor wired to a fresh pool would
/// not inherit the predecessor's occupancy, and the outage this file reproduces
/// is precisely that the occupancy DOES survive the restart.
async fn restart_and_reconcile(
    harness: &CapacityHarness,
    cap: i64,
) -> Arc<BuildAdmissionController> {
    let successor = Arc::new(
        BuildAdmissionController::new(
            Arc::clone(&harness.journal),
            BuildAdmissionMode::Enforce,
            cap,
            SUCCESSOR,
        )
        .with_slot_authority(Arc::new(BuildLeaseDispatchAuthority::new(Arc::clone(
            &harness.lease,
        )))),
    );
    successor
        .recover_all_predecessors_and_seed()
        .await
        .expect("durable journal recovery");
    let report = BuildAdmissionReconciler::with_settle_window(
        Arc::clone(&successor),
        Arc::new(EmptyNamespace),
        Duration::ZERO,
    )
    .reconcile()
    .await;
    assert!(
        report.blockers.is_empty(),
        "blockers: {:?}",
        report.blockers
    );
    successor
}

fn reclaimer(harness: &CapacityHarness) -> BuildLeaseReclaimer {
    BuildLeaseReclaimer::with_settle_window(
        Arc::clone(&harness.leases),
        Arc::clone(&harness.journal),
        Arc::new(EmptyNamespace),
        Duration::ZERO,
    )
}

async fn harness(cap: i64) -> CapacityHarness {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    controller_with_capacity_over(&db, BuildAdmissionMode::Enforce, cap, PREDECESSOR).await
}

/// The whole outage: a dispatch slot outlives its task-run, every later task is
/// denied at a cap nothing is really using, and reclamation is what gives the
/// board back.
#[tokio::test]
async fn dispatch_leak_wedges_every_task_until_it_is_reclaimed() {
    let harness = harness(1).await;
    dispatch_task_run(&harness, "wedging-task").await;
    assert_eq!(
        harness.occupancy().await,
        1,
        "the dispatch attempt must occupy a real build slot"
    );

    // The pod dies during a failed rollout; the replacement recovers. This is
    // the pass that logged `stale_rows=2 reclaimed=2` in production.
    let successor = restart_and_reconcile(&harness, 1).await;
    assert_eq!(
        harness
            .journal
            .generation_state(AdmissionDomain::TaskObservation, "wedging-task", 0)
            .await
            .unwrap(),
        Some(AdmissionState::Terminal),
        "the v0 lifecycle ledger released the generation on a Kubernetes absence proof"
    );
    assert_eq!(
        harness.ledger_rows().await,
        0,
        "no lifecycle row occupies anything any more"
    );

    // ...and the capacity ledger did not move at all. This is the defect.
    assert_eq!(
        harness.occupancy().await,
        1,
        "the v1 capacity ledger still holds the slot: no reclaimer could see it"
    );
    assert_eq!(
        harness
            .leases
            .get(&lease_key("wedging-task", 0))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Launching,
        "a properly acknowledged dispatch slot, orphaned: only its holder could \
         release it, and its holder is gone"
    );
    assert!(
        matches!(
            admit(&successor, "next-task").await,
            BuildAdmissionDecision::Denied {
                occupancy: Some(1),
                cap: 1,
                cause: DenialCause::AtCapacity
            }
        ),
        "the production symptom: every task denied against occupancy it cannot use"
    );

    // Reclamation, on the proof that the generation this slot was bought for is
    // over.
    let report = reclaimer(&harness).reclaim().await;
    assert!(
        report.blockers.is_empty(),
        "blockers: {:?}",
        report.blockers
    );
    assert!(
        report.failures.is_empty(),
        "failures: {:?}",
        report.failures
    );

    // The side effects come FIRST, deliberately. A reclaimer that returns a
    // convincing report and frees nothing is the exact failure mode this suite
    // exists to catch, so neutralizing reclamation must fail here — on the
    // capacity the cap is compared against and on a real admission — rather
    // than on a counter.
    assert_eq!(
        harness.occupancy().await,
        0,
        "occupancy must return to zero: this is the side effect, not the report"
    );
    assert!(
        matches!(
            admit(&successor, "next-task").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "and the next task must actually be PERMITTED, not merely counted"
    );
    assert_eq!(harness.occupancy().await, 1, "by taking the freed slot");

    // Corroboration only.
    assert_eq!(report.occupying, 1);
    assert_eq!(report.ownerless_dispatch, 1);
    assert_eq!(report.reclaimed, 1);
    assert_eq!(report.fenced, 0);
}

/// The negative that matters most: a dispatch slot whose generation is still
/// running is live work, and no amount of age or Kubernetes emptiness makes it
/// reclaimable.
///
/// The empty namespace here is deliberate. A task-run Job's name is derived
/// from a `task_run_id` that does not exist when the slot is bought, so an
/// empty listing says nothing at all about a dispatch lease. If reclamation
/// ever starts reading the namespace for this population, this test fails.
#[tokio::test]
async fn a_dispatch_slot_whose_generation_still_occupies_is_never_retired() {
    let harness = harness(2).await;
    dispatch_task_run(&harness, "running-task").await;
    assert_eq!(
        harness
            .journal
            .generation_state(AdmissionDomain::TaskObservation, "running-task", 0)
            .await
            .unwrap(),
        Some(AdmissionState::CreateUnknown)
    );

    let report = reclaimer(&harness).reclaim().await;
    assert_eq!(report.occupying, 1);
    assert_eq!(
        report.absent, 0,
        "an occupying generation is not a proof of anything"
    );
    assert_eq!(report.reclaimed, 0);
    assert_eq!(harness.occupancy().await, 1);
}

/// The second dispatch leak: the FIFO grants a slot to a requester that already
/// gave up on a `Queued` answer, and nobody is left to hand it back.
///
/// Produced entirely through the production composition — a denial at a full
/// cap leaves a queued position, the holder's release plus a later request
/// drains the FIFO onto that position, and nothing ever acknowledges it. The
/// resulting row is `granted` (never `launching`, because `granted` is exactly
/// the state a live dispatch has already left) with no ledger row behind it.
#[tokio::test]
async fn a_grant_the_fifo_handed_to_nobody_is_retired_and_gives_the_cap_back() {
    let harness = harness(1).await;

    // The holder takes the only slot, then fails before the pool creates
    // anything, which is the production `DefinitiveFailure` release.
    let holder = permitted(&harness.controller, "holder").await;
    harness
        .controller
        .transition(&holder, WarmAdmissionTransition::CreateStarted)
        .await
        .expect("record the create");
    assert!(
        matches!(
            admit(&harness.controller, "abandoned").await,
            BuildAdmissionDecision::Denied { .. }
        ),
        "the second task must be refused behind the full cap, leaving a queue position"
    );
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

    // A later task's own queue drains the FIFO — onto the abandoned position,
    // not onto itself. This is the wedge.
    assert!(matches!(
        admit(&harness.controller, "live").await,
        BuildAdmissionDecision::Denied { .. }
    ));
    let abandoned = harness
        .leases
        .get(&lease_key("abandoned", 0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        abandoned.state,
        BuildLeaseState::Granted,
        "granted and never acknowledged: no dispatch is holding it"
    );
    assert_eq!(harness.occupancy().await, 1);
    assert_eq!(
        harness
            .journal
            .generation_state(AdmissionDomain::TaskObservation, "abandoned", 0)
            .await
            .unwrap(),
        None,
        "no admission ever proceeded on this grant"
    );

    let report = reclaimer(&harness).reclaim().await;
    assert_eq!(harness.occupancy().await, 0);
    assert!(
        matches!(
            admit(&harness.controller, "live").await,
            BuildAdmissionDecision::Permitted { .. }
        ),
        "the waiting task must be permitted once the phantom grant is retired"
    );
    assert_eq!(report.ownerless_dispatch, 1);
    assert_eq!(report.reclaimed, 1);
}

/// A dispatch lease that IS held — acknowledged into `launching` — but whose
/// ledger row cannot be found is UNKNOWN state, not proven ownerless.
///
/// `BuildAdmissionController::admit` writes no journal row while v0 is `off`,
/// so an absent row can never by itself mean "nobody owns this". Fail-closed
/// here is a deliberate trade; the grant-never-claimed case above is the only
/// shape where an absent row IS conclusive, because a live dispatch has always
/// left `granted` behind.
#[tokio::test]
async fn a_held_dispatch_lease_with_no_ledger_row_stays_occupying() {
    let harness = harness(1).await;
    let permit = permitted(&harness.controller, "unledgered").await;
    harness
        .controller
        .transition(&permit, WarmAdmissionTransition::CreateStarted)
        .await
        .expect("record the create");
    assert_eq!(
        harness
            .leases
            .get(&lease_key("unledgered", 0))
            .await
            .unwrap()
            .unwrap()
            .state,
        BuildLeaseState::Launching,
        "an acknowledged dispatch slot is past `granted`"
    );

    // A journal that holds no row for this generation, as v0 `off` would leave.
    let empty_ledger = Arc::new(AdmissionJournalRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let report = BuildLeaseReclaimer::with_settle_window(
        Arc::clone(&harness.leases),
        empty_ledger,
        Arc::new(EmptyNamespace),
        Duration::ZERO,
    )
    .reclaim()
    .await;
    assert_eq!(report.occupying, 1);
    assert_eq!(report.absent, 0);
    assert_eq!(report.reclaimed, 0);
    assert_eq!(harness.occupancy().await, 1);
}

/// A lease that has not settled is not judged at all: a dispatch still mid-flight
/// must be allowed to write its ledger row first.
#[tokio::test]
async fn an_unsettled_dispatch_lease_is_not_judged() {
    let harness = harness(1).await;
    dispatch_task_run(&harness, "fresh-task").await;
    restart_and_reconcile(&harness, 1).await;

    let report = BuildLeaseReclaimer::with_settle_window(
        Arc::clone(&harness.leases),
        Arc::clone(&harness.journal),
        Arc::new(EmptyNamespace),
        Duration::from_secs(3600),
    )
    .reclaim()
    .await;
    assert_eq!(report.occupying, 1);
    assert_eq!(report.reclaimed, 0);
    assert_eq!(harness.occupancy().await, 1);
}

/// The generation a dispatch lease owns is read from its durable identity, and
/// an identity that does not parse yields nothing rather than a guess.
#[test]
fn dispatch_identities_come_from_the_durable_row_or_nowhere() {
    let row = |kind, identity: &str| djinn_db::BuildLeaseRow {
        key: BuildLeaseKey {
            consumer_kind: kind,
            consumer_id: "consumer".into(),
        },
        immutable_identity: identity.into(),
        enqueue_sequence: 1,
        fencing_token: Some(1),
        state: BuildLeaseState::Granted,
        queue_deadline: None,
        launch_deadline: None,
        bound_pod_uid: None,
        candidate_cleanup: None,
        terminal_reason: None,
        weight: 1,
        timeout_credit_consumed: false,
        created_at: "now".into(),
        updated_at: "now".into(),
        granted_at: None,
        terminal_at: None,
    };
    assert_eq!(
        dispatch_identity(&row(
            BuildLeaseConsumerKind::TaskDispatch,
            "dispatch:019ea3bd-a305-73e3-806c-4edcc96ebfe2:4"
        )),
        Some(("019ea3bd-a305-73e3-806c-4edcc96ebfe2".to_owned(), 4))
    );
    // A warm or invocation lease is never read as a dispatch identity, even if
    // its identity string happened to look like one.
    assert_eq!(
        dispatch_identity(&row(BuildLeaseConsumerKind::GraphWarm, "dispatch:task:0")),
        None
    );
    for malformed in [
        "dispatch:task-1",
        "dispatch::0",
        "dispatch:task-1:not-a-number",
        "dispatch:task-1:-1",
        "warm:project:request:revision",
    ] {
        assert_eq!(
            dispatch_identity(&row(BuildLeaseConsumerKind::TaskDispatch, malformed)),
            None,
            "{malformed} must not resolve to a generation"
        );
    }
}
