//! The one way a test composes build capacity, shared by every admission suite.
//!
//! # Why this module exists
//!
//! Capacity used to be decided by the v0 admission journal, so a test could
//! construct a bare [`BuildAdmissionController`] over an in-memory journal and
//! get a real, enforcing cap for free. That is no longer true: the journal is a
//! lifecycle ledger and cannot deny, and occupancy is the weighted sum over
//! `build_leases` read by the single [`BuildSlotAuthority`]. A controller built
//! without an authority is therefore **ungated** — every admission is permitted
//! — which is the correct production shape for the local-dev/Off composition
//! and a silently vacuous one for a test that means to assert a cap.
//!
//! Rather than let every suite re-derive the wiring (which is how a subtly
//! wrong fixture lands green), every capacity-asserting test in this crate
//! composes through [`CapacityHarness`], which assembles exactly what
//! `AppState` assembles: ONE recovered, armed [`BuildLeaseService`] behind ONE
//! controller, over ONE database.
//!
//! # The two units these tests must not confuse
//!
//! * A **journal row** records one object's lifecycle. It reserves nothing.
//!   `count_task_or_warm_occupancy()` counts rows.
//! * A **build slot** is CPU. It is the weighted sum over occupying
//!   `build_leases` rows, read with [`CapacityHarness::occupancy`].
//!
//! Asserting a row count against a build-slot cap compiles and means nothing.
//! Tests that guard a cap assert [`CapacityHarness::occupancy`]; tests that
//! guard the ledger assert the journal.

use std::sync::Arc;

use djinn_db::{AdmissionJournalRepository, BuildLeaseRepository, Database};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseAbandonRequest, LeaseDeadlines, LeaseFencingToken, LeaseIdentity,
    LeaseQueueRequest, LeaseReleaseRequest, LeaseResult,
};

use crate::build_admission::{BuildAdmissionController, BuildAdmissionMode, BuildSlotAuthority};
use crate::build_lease::{BuildLeaseService, BuildSlotWeights};
use crate::build_slot_authority::BuildLeaseDispatchAuthority;

/// A controller wired to the single capacity authority, plus every ledger the
/// assertions need to read.
pub(crate) struct CapacityHarness {
    pub(crate) journal: Arc<AdmissionJournalRepository>,
    pub(crate) leases: Arc<BuildLeaseRepository>,
    pub(crate) lease: Arc<BuildLeaseService>,
    pub(crate) controller: Arc<BuildAdmissionController>,
}

impl CapacityHarness {
    /// Occupied build SLOTS, from the one authority. This — never a journal row
    /// count — is what a cap is compared against.
    pub(crate) async fn occupancy(&self) -> i64 {
        self.leases.snapshot().await.unwrap().occupied
    }

    /// Occupying journal rows: the lifecycle ledger, which reserves nothing.
    pub(crate) async fn ledger_rows(&self) -> i64 {
        self.journal.count_task_or_warm_occupancy().await.unwrap()
    }

    /// Take a graph-warm lease exactly as the warmer does *before* it reaches
    /// admission, returning the release handle when capacity was granted.
    ///
    /// Production order matters here and is not a detail: a warm contender that
    /// skipped this presents `CapacitySource::HeldByLease` while holding
    /// nothing, is never capacity-checked at all, and makes the surrounding
    /// assertion vacuous.
    pub(crate) async fn hold_warm_lease(&self, request: &str) -> Option<HeldWarmLease> {
        let identity = LeaseIdentity::GraphWarm(GraphWarmLeaseIdentity {
            project_id: "capacity-harness".into(),
            warm_request_id: request.to_owned(),
            graph_revision: "capacity-harness-revision".into(),
        });
        match self
            .lease
            .queue(LeaseQueueRequest {
                identity: identity.clone(),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await
        {
            LeaseResult::Granted(grant) => Some(HeldWarmLease {
                identity,
                fencing_token: grant.fencing_token,
            }),
            // Lost the race. Surrender the FIFO position rather than leaving it
            // queued: a peer's later release drains the FIFO, GRANTS this
            // abandoned row, and leaks the slot with nobody left to release it.
            _ => {
                let _ = self
                    .lease
                    .abandon(LeaseAbandonRequest {
                        identity,
                        candidate_cleanup: false,
                    })
                    .await;
                None
            }
        }
    }

    /// Occupy `slots` build slots even though the cap under test is smaller,
    /// then restore that cap.
    ///
    /// This reproduces the one shape a fresh pool cannot: capacity a PREDECESSOR
    /// took, surviving in `build_leases` across the restart, against a cap the
    /// operator has since lowered. Grants go through the real service rather
    /// than inserting rows, so the weights and states are the ones production
    /// writes.
    pub(crate) async fn occupy_slots_beyond_cap(&self, slots: i64) -> Vec<HeldWarmLease> {
        let cap = self.lease.cap();
        assert!(matches!(
            self.lease.set_cap(cap.max(slots)).await,
            LeaseResult::Status(_)
        ));
        let mut held = Vec::new();
        for index in 0..slots {
            held.push(
                self.hold_warm_lease(&format!("predecessor-slot-{index}"))
                    .await
                    .expect("the widened pool must grant every predecessor slot"),
            );
        }
        assert!(matches!(
            self.lease.set_cap(cap).await,
            LeaseResult::Status(_)
        ));
        held
    }

    /// Hand a held graph-warm lease back, as the warmer does when its Job ends.
    pub(crate) async fn release_warm_lease(&self, held: HeldWarmLease) {
        let _ = self
            .lease
            .release(LeaseReleaseRequest {
                identity: held.identity,
                fencing_token: held.fencing_token,
                candidate_cleanup: false,
            })
            .await;
    }
}

/// A graph-warm lease this test currently occupies.
pub(crate) struct HeldWarmLease {
    identity: LeaseIdentity,
    fencing_token: LeaseFencingToken,
}

/// Compose the production capacity authority over `db` at `cap` build slots.
///
/// Recovered and armed, because an unrecovered service reports occupancy as
/// unknown and an unarmed one only shadows — both of which turn a cap assertion
/// into a no-op.
pub(crate) async fn armed_lease_service(
    leases: &Arc<BuildLeaseRepository>,
    cap: i64,
) -> Arc<BuildLeaseService> {
    let lease = Arc::new(
        BuildLeaseService::new(Arc::clone(leases), cap)
            .with_slot_weights(BuildSlotWeights::default()),
    );
    assert!(
        matches!(lease.recover().await, LeaseResult::Status(_)),
        "the capacity authority must recover before a test may assert a cap"
    );
    assert!(
        matches!(lease.set_cap(cap).await, LeaseResult::Status(_)),
        "the capacity authority must arm the cap under test"
    );
    // In production this is the durable admission epoch reaching
    // invocation-primary; nothing else can arm layer-1 dispatch.
    lease.set_dispatch_enforcing_for_test(true);
    lease
}

/// Attach the single capacity authority to an already-constructed controller.
///
/// Used by the suites that need a non-default controller shape (a fake queue
/// clock, `new_closed`, a specific epoch) and cannot go through
/// [`controller_with_capacity`].
pub(crate) async fn attach_capacity(
    db: &Database,
    controller: BuildAdmissionController,
    cap: i64,
) -> CapacityHarness {
    let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
    let lease = armed_lease_service(&leases, cap).await;
    let authority: Arc<dyn BuildSlotAuthority> =
        Arc::new(BuildLeaseDispatchAuthority::new(Arc::clone(&lease)));
    CapacityHarness {
        journal,
        leases,
        lease,
        controller: Arc::new(controller.with_slot_authority(authority)),
    }
}

/// The default shape: a fresh in-memory database, one journal, one armed lease
/// authority, and a controller that reaches capacity through it.
pub(crate) async fn controller_with_capacity(
    mode: BuildAdmissionMode,
    cap: i64,
    epoch: &str,
) -> CapacityHarness {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    controller_with_capacity_over(&db, mode, cap, epoch).await
}

/// Same, over a caller-supplied database, for tests that must seed rows first
/// or share the database with a second component (a warmer, a dispatcher).
pub(crate) async fn controller_with_capacity_over(
    db: &Database,
    mode: BuildAdmissionMode,
    cap: i64,
    epoch: &str,
) -> CapacityHarness {
    let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
    let controller = BuildAdmissionController::new(Arc::clone(&journal), mode, cap, epoch);
    attach_capacity(db, controller, cap).await
}
