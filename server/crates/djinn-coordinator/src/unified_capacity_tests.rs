//! Proof that the v1 build-lease FIFO is the ONE authority over build capacity.
//!
//! This file used to prove that TWO authorities -- the pre-create admission
//! journal and the v1 build lease -- shared a single cap instead of each
//! enforcing `DJINN_MAX_BUILD_TASKRUNS` over its own disjoint population (the
//! 2x-concurrency defect). Task `o53p` deleted the pre-create reservation
//! authority outright, so the "two authorities agree" claim is now vacuous and
//! its tests are gone with the controller they drove.
//!
//! What survives is the half that still has a subject: the lease FIFO itself
//! and the weight derivation that prices rows in it. Every test below drives the
//! real [`BuildLeaseRepository`] against a fresh Postgres, or derives weight
//! from the real rendered manifests. Nothing is faked -- the defect class these
//! guard against is "the fixture supplied what production does not".

use djinn_db::{BuildLeaseConsumerKind, BuildLeaseRepository, BuildLeaseState, Database};
use djinn_runtime::{BuildSlotWeight, RoleResourceClass};

use crate::build_lease::BuildSlotWeights;

/// Strict FIFO by weight: a heavy head is never skipped for a lighter row
/// behind it. Skipping would starve a heavy consumer behind the unbroken stream
/// of weight-1 dispatches that arrives most often.
#[tokio::test]
async fn a_heavy_queue_head_is_not_starved_by_lighter_rows_behind_it() {
    use djinn_db::{BuildLeaseKey, GrantNextBuildLeaseResult, QueueBuildLeaseInput};

    let db = Database::open_in_memory().unwrap();
    let leases = BuildLeaseRepository::new(db);
    let now = "2026-07-25T00:00:00Z";

    let queue = async |kind, id: &str, weight| {
        leases
            .queue(&QueueBuildLeaseInput {
                key: BuildLeaseKey {
                    consumer_kind: kind,
                    consumer_id: id.into(),
                },
                immutable_identity: format!("identity:{id}"),
                queue_deadline: None,
                launch_deadline: None,
                weight,
            })
            .await
            .unwrap()
    };

    // FIFO order matters, so the occupant is enqueued FIRST. Then a weight-2
    // warm Job (a deployment that raised DJINN_K8S_WARM_CPU_REQUEST), then a
    // light dispatch behind it.
    queue(BuildLeaseConsumerKind::TaskDispatch, "occupant", 1).await;
    queue(BuildLeaseConsumerKind::GraphWarm, "heavy-head", 2).await;
    queue(BuildLeaseConsumerKind::TaskDispatch, "light-behind", 1).await;

    // Cap 2: the occupant takes one unit, so the weight-2 head no longer fits
    // while the weight-1 row behind it would. Strict FIFO must grant neither.
    assert!(matches!(
        leases.grant_next(2, now, None).await.unwrap(),
        GrantNextBuildLeaseResult::Granted(_)
    ));

    match leases.grant_next(2, now, None).await.unwrap() {
        GrantNextBuildLeaseResult::Empty { occupancy, cap } => {
            assert_eq!((occupancy, cap), (1, 2));
        }
        GrantNextBuildLeaseResult::Granted(row) => panic!(
            "granted {:?} past a heavy head that did not fit -- the head starves",
            row.key.consumer_id
        ),
    }
}

/// Occupancy is a weighted SUM, not a row COUNT. A COUNT would make the
/// zero-weight re-entry occupy a full slot and reintroduce double-charging.
#[tokio::test]
async fn occupancy_sums_weight_rather_than_counting_rows() {
    use djinn_db::{BuildLeaseKey, GrantNextBuildLeaseResult, QueueBuildLeaseInput};

    let db = Database::open_in_memory().unwrap();
    let leases = BuildLeaseRepository::new(db);
    let now = "2026-07-25T00:00:00Z";

    for (id, weight) in [("zero-a", 0), ("zero-b", 0), ("real", 1)] {
        leases
            .queue(&QueueBuildLeaseInput {
                key: BuildLeaseKey {
                    consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
                    consumer_id: id.into(),
                },
                immutable_identity: format!("identity:{id}"),
                queue_deadline: None,
                launch_deadline: None,
                weight,
            })
            .await
            .unwrap();
    }
    for _ in 0..3 {
        assert!(matches!(
            leases.grant_next(1, now, None).await.unwrap(),
            GrantNextBuildLeaseResult::Granted(_)
        ));
    }

    let snapshot = leases.snapshot().await.unwrap();
    assert_eq!(
        snapshot.occupied, 1,
        "three occupying rows weighing 0+0+1 must occupy ONE slot, not three"
    );
    assert_eq!(
        snapshot
            .rows
            .iter()
            .filter(|row| row.state != BuildLeaseState::Terminal)
            .count(),
        3
    );
}

/// Weight is derived from the rendered manifests, not typed into admission.
///
/// The premise this replaces was that a graph-warm Job (a full workspace
/// compile) must outweigh a task-run that only might compile briefly. Measured
/// against the real render that is false: both request 4000m. A concurrency
/// semaphore governs RATE, not duration, and while they run these two cost the
/// node the same. If the render ever diverges, this fails.
#[test]
fn build_slot_weight_is_derived_from_the_real_rendered_cpu() {
    let config = djinn_k8s::KubernetesConfig::for_testing();
    let slot = djinn_k8s::launcher::launcher_leased_millicores(&config);
    let warm = djinn_k8s::launcher::warm_job_millicores(&config);

    assert_eq!(slot, 4_000, "the leased task-run quota changed");
    assert_eq!(warm, 4_000, "the warm Job CPU request changed");

    let weights = BuildSlotWeights {
        slot_millicores: slot,
        warm_millicores: warm,
    };
    assert_eq!(weights.warm().slots(), 1);
    assert_eq!(weights.dispatch().slots(), 1);
    assert_eq!(
        weights.invocation(false).slots(),
        1,
        "an invocation with no dispatch slot behind it pays in full"
    );
    assert_eq!(
        weights.invocation(true).slots(),
        0,
        "an invocation whose slot is already held must be free"
    );

    // The derivation is real, not a constant: doubling the warm request
    // reweights it with no code change.
    let doubled = BuildSlotWeights {
        slot_millicores: slot,
        warm_millicores: warm * 2,
    };
    assert_eq!(doubled.warm().slots(), 2);

    // Rounding is UP, so a workload asking for more than a slot occupies more
    // than a slot rather than being made cheap by truncation.
    assert_eq!(BuildSlotWeight::for_millicores(4_001, 4_000).slots(), 2);
    assert_eq!(BuildSlotWeight::for_millicores(1, 4_000).slots(), 1);
    assert_eq!(BuildSlotWeight::for_millicores(0, 4_000).slots(), 0);
}

/// Light roles buy nothing at dispatch; build-capable roles buy one slot.
#[test]
fn dispatch_weight_follows_the_role_resource_class() {
    assert_eq!(
        BuildSlotWeight::for_dispatch(RoleResourceClass::Light, 4_000).slots(),
        0
    );
    assert_eq!(
        BuildSlotWeight::for_dispatch(RoleResourceClass::BuildCapable, 4_000).slots(),
        1
    );
}
