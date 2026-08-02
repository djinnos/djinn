//! `epoch set-cap` against a RUNNING process.
//!
//! Production, 2026-07-25, mid-incident: `djinn-server epoch set-cap --cap 12`
//! reported `set-cap: applied`, `djinn-server epoch show` read back `cap 12`,
//! and the very next denial still said `occupancy=3 cap=3`. The durable write
//! landed; the live `grant_next` read a cached atomic that only `recover()`
//! ever wrote, so the operator's one cap knob was inert until a restart.
//!
//! Every assertion below is on an ENFORCED DECISION — a lease that was queued
//! behind the old cap becoming granted, or a request refused at the new one —
//! never on reading the durable column back. Reading the column back is exactly
//! what made the defect look fixed in the first place.

use std::sync::Arc;

use djinn_db::{
    BuildLeaseKey, BuildLeaseRepository, BuildLeaseState, Database,
    InvocationLeaseAuthorityRepository, InvocationLeaseMode,
};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseDeadlines, LeaseIdentity, LeaseQueueRequest, LeaseResult,
};

use crate::build_lease::{BuildLeaseService, DampedCapacitySnapshot};
use crate::invocation_lease_control::InvocationLeaseControl;

const PROJECT: &str = "019ea3bd-a305-73e3-806c-4edcc96ebfe2";

struct Fixture {
    service: Arc<BuildLeaseService>,
    leases: Arc<BuildLeaseRepository>,
    operator: InvocationLeaseControl,
    epoch: i64,
    authority: Arc<InvocationLeaseAuthorityRepository>,
}

impl Fixture {
    /// Run the operator command the runbook runs: `djinn-server epoch set-cap`.
    /// This is the same executor `server/src/admin.rs` drives, so nothing here
    /// simulates the write path.
    async fn operator_set_cap(&self, cap: i64) {
        self.operator
            .set_cap(self.epoch, cap)
            .await
            .expect("the durable cap write must succeed");
    }

    async fn queue_warm(&self, request: &str) -> LeaseResult {
        self.service
            .queue(LeaseQueueRequest {
                identity: LeaseIdentity::GraphWarm(GraphWarmLeaseIdentity {
                    project_id: PROJECT.into(),
                    warm_request_id: request.into(),
                    graph_revision: format!("rev-{request}"),
                }),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            })
            .await
    }

    async fn state(&self, request: &str) -> BuildLeaseState {
        self.leases
            .get(&BuildLeaseKey {
                consumer_kind: djinn_db::BuildLeaseConsumerKind::GraphWarm,
                consumer_id: request.into(),
            })
            .await
            .unwrap()
            .expect("the lease row must exist")
            .state
    }

    async fn occupied(&self) -> i64 {
        self.leases.snapshot().await.unwrap().occupied
    }
}

/// A running process that has already recovered against an armed epoch cap,
/// exactly as `AppState` composes it.
async fn running_process(epoch_cap: i64) -> Fixture {
    let db = Database::ephemeral().await.unwrap();
    let authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
    let row = authority
        .set_mode_and_cap(0, InvocationLeaseMode::Enforce, Some(epoch_cap))
        .await
        .expect("arm the epoch");
    let leases = Arc::new(BuildLeaseRepository::new(db.clone()));
    let service = Arc::new(
        // The configured `DJINN_MAX_BUILD_TASKRUNS` fallback is deliberately
        // different from the epoch cap, so any assertion below that passes by
        // reading the environment instead of the epoch is visible.
        BuildLeaseService::new(Arc::clone(&leases), 9)
            .with_invocation_lease_authority(Arc::clone(&authority)),
    );
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert_eq!(
        service.cap(),
        epoch_cap,
        "the armed epoch cap outranks the configured fallback at startup"
    );
    Fixture {
        service,
        leases,
        operator: InvocationLeaseControl::new(Arc::clone(&authority)),
        epoch: row.epoch,
        authority,
    }
}

/// The defect, and the fix, in one: a durable `set-cap` must change what the
/// live process ENFORCES, and must unblock work that was refused at the old cap
/// without waiting for the next request.
#[tokio::test]
async fn a_durable_set_cap_changes_what_the_running_process_enforces() {
    let fixture = running_process(1).await;

    assert!(matches!(
        fixture.queue_warm("holder").await,
        LeaseResult::Granted(_)
    ));
    assert!(
        matches!(fixture.queue_warm("waiter").await, LeaseResult::Queued(_)),
        "cap 1 is genuinely enforced before the operator touches anything"
    );
    assert_eq!(fixture.occupied().await, 1);

    // The incident action. It reports success and the durable row now reads 2.
    fixture.operator_set_cap(2).await;
    assert_eq!(
        fixture.service.cap(),
        1,
        "precondition: a durable write alone does not reach the cached cap — \
         this is precisely what made the production knob inert"
    );

    assert_eq!(
        fixture.service.refresh_epoch_cap().await,
        Some(2),
        "the live process must adopt the durable cap without a restart"
    );
    assert_eq!(fixture.service.cap(), 2);
    assert_eq!(
        fixture.state("waiter").await,
        BuildLeaseState::Granted,
        "raising the cap must DRAIN: the lease refused at cap 1 is granted now, \
         not at whatever time something else happens to queue"
    );
    assert_eq!(fixture.occupied().await, 2);
    assert!(
        matches!(fixture.queue_warm("third").await, LeaseResult::Queued(_)),
        "and the new cap is then enforced as the real ceiling"
    );
}

/// Lowering the cap stops granting but never revokes capacity an occupant is
/// already using: an in-flight build is not killed by an operator typo.
#[tokio::test]
async fn lowering_the_cap_stops_granting_without_revoking_occupied_slots() {
    let fixture = running_process(2).await;
    for request in ["first", "second"] {
        assert!(matches!(
            fixture.queue_warm(request).await,
            LeaseResult::Granted(_)
        ));
    }
    assert_eq!(fixture.occupied().await, 2);

    fixture.operator_set_cap(1).await;
    assert_eq!(fixture.service.refresh_epoch_cap().await, Some(1));
    assert_eq!(
        fixture.occupied().await,
        2,
        "occupied slots are never revoked by a cap change"
    );
    for request in ["first", "second"] {
        assert_eq!(fixture.state(request).await, BuildLeaseState::Granted);
    }
    assert!(
        matches!(fixture.queue_warm("third").await, LeaseResult::Queued(_)),
        "the lowered cap is enforced against NEW requests"
    );
}

/// A refresh before recovery has opened the service changes nothing. Occupancy
/// is unknown then, and unknown is never a reason to move what is enforced.
#[tokio::test]
async fn a_refresh_before_recovery_never_moves_the_enforced_cap() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
    authority
        .set_mode_and_cap(0, InvocationLeaseMode::Enforce, Some(7))
        .await
        .expect("arm the epoch");
    let service = BuildLeaseService::new(Arc::new(BuildLeaseRepository::new(db.clone())), 3)
        .with_invocation_lease_authority(authority);

    assert_eq!(service.refresh_epoch_cap().await, None);
    assert_eq!(
        service.cap(),
        3,
        "an unrecovered service keeps its configured cap rather than adopting an \
         epoch it has not yet reconciled occupancy against"
    );
}

#[tokio::test]
async fn build_lease_cap_refresh_adopts_damped_fallback_without_restart() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
    authority
        .set_mode_and_cap(0, InvocationLeaseMode::Enforce, None)
        .await
        .unwrap();
    let service = BuildLeaseService::new(Arc::new(BuildLeaseRepository::new(db)), 2)
        .with_invocation_lease_authority(authority);
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert_eq!(
        service
            .adopt_capacity_snapshot(DampedCapacitySnapshot::Capacity { compile_slots: 12 })
            .await,
        Some(12)
    );
    assert_eq!(service.cap(), 12);
    assert_eq!(
        service
            .adopt_capacity_snapshot(DampedCapacitySnapshot::Conservative {
                fail_safe_compile_slots: 2
            })
            .await,
        Some(2)
    );
    assert_eq!(service.cap(), 2);
}

#[tokio::test]
async fn build_lease_cap_refresh_requires_damped_48_core_agreement_and_dwell() {
    use djinn_k8s::capacity_damping::{
        BindingQuota, CapacityDamper, CapacityVector, MINIMUM_GROWTH_DWELL, SampleKind,
    };
    use tokio::time::Instant;

    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let authority = Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
    authority
        .set_mode_and_cap(0, InvocationLeaseMode::Enforce, None)
        .await
        .unwrap();
    let service = BuildLeaseService::new(Arc::new(BuildLeaseRepository::new(db)), 2)
        .with_invocation_lease_authority(authority);
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));

    let start = Instant::now();
    let mut damper = CapacityDamper::new(BindingQuota::Pods(10), 2, start);
    let high = CapacityVector {
        binding: BindingQuota::Pods(48),
        compile_slots: 12,
    };
    let first = damper.observe(high, SampleKind::Periodic, start + MINIMUM_GROWTH_DWELL);
    assert_eq!(first.compile_slots, 2, "one high snapshot cannot widen");
    service
        .adopt_capacity_snapshot(DampedCapacitySnapshot::Capacity {
            compile_slots: first.compile_slots,
        })
        .await;
    assert_eq!(service.cap(), 2);
    damper.observe(
        high,
        SampleKind::Periodic,
        start + MINIMUM_GROWTH_DWELL + std::time::Duration::from_secs(30),
    );
    let agreed = damper.observe(
        high,
        SampleKind::Periodic,
        start + MINIMUM_GROWTH_DWELL + std::time::Duration::from_secs(60),
    );
    service
        .adopt_capacity_snapshot(DampedCapacitySnapshot::Capacity {
            compile_slots: agreed.compile_slots,
        })
        .await;
    assert_eq!(service.cap(), 12);
}

#[tokio::test]
async fn build_lease_cap_durable_override_remains_authoritative() {
    let fixture = running_process(7).await;
    assert_eq!(
        fixture
            .service
            .adopt_capacity_snapshot(DampedCapacitySnapshot::Capacity { compile_slots: 12 })
            .await,
        Some(7)
    );
    assert_eq!(
        fixture
            .service
            .adopt_capacity_snapshot(DampedCapacitySnapshot::Conservative {
                fail_safe_compile_slots: 2
            })
            .await,
        Some(7)
    );
    assert_eq!(fixture.service.cap(), 7);

    fixture
        .authority
        .set_mode_and_cap(fixture.epoch, InvocationLeaseMode::Enforce, None)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .service
            .adopt_capacity_snapshot(DampedCapacitySnapshot::Capacity { compile_slots: 12 })
            .await,
        Some(12),
        "clearing the operator override adopts the latest damped fallback"
    );
}
