//! Cap arming for the one v1 build-lease FIFO.
//!
//! `build_lease_caps` is seeded to 0 by migration 139 and is written only by
//! `set_cap`/`grant_next`. A deployment that never ran the operator arming
//! sequence therefore keeps a durable cap of 0, and `grant_next` short-circuits
//! on `occupied >= cap` before it ever reads the queue — every consumer denied
//! for the life of the process while every occupancy gauge reads zero.
//!
//! This is the production outage these tests pin: the graph-warm FIFO held only
//! queued and deadline-expired rows, zero occupying rows, zero task-invocation
//! rows, and zero warm Jobs created in the days since the lease was wired.
//!
//! Every test drives the real adapter over the real repository against a fresh
//! database, which reproduces that durable state exactly.

use std::sync::Arc;

use djinn_db::{BuildLeaseRepository, Database};
use djinn_k8s::GraphWarmLease;
use djinn_supervisor::services::{GraphWarmLeaseIdentity, LeaseDeadlines, LeaseResult};

use crate::build_lease::BuildLeaseService;
use crate::graph_warm_lease::BuildLeaseGraphWarmAdapter;

async fn unarmed_production_database() -> (Database, Arc<BuildLeaseRepository>) {
    let database = Database::open_in_memory().unwrap();
    database.ensure_initialized().await.unwrap();
    let repository = Arc::new(BuildLeaseRepository::new(database.clone()));
    assert_eq!(
        repository.snapshot().await.unwrap().cap,
        0,
        "precondition: the migration seeds an unarmed durable cap of 0"
    );
    (database, repository)
}

#[tokio::test]
async fn an_unarmed_durable_cap_denies_every_graph_warm_lease_forever() {
    use djinn_db::AdmissionHandoffRepository;

    let (database, repository) = unarmed_production_database().await;
    let handoff = Arc::new(AdmissionHandoffRepository::new(database.clone()));
    // The composition as it shipped: the configured build-slot cap was never
    // handed to the lease service, so the resolution chain bottomed out at the
    // unarmed durable 0.
    let service = Arc::new(
        BuildLeaseService::new(Arc::clone(&repository), 0).with_handoff_epoch(handoff),
    );
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert_eq!(service.cap(), 0);

    let adapter = BuildLeaseGraphWarmAdapter::new(Arc::clone(&service));
    for attempt in 0..3 {
        assert!(
            matches!(
                adapter
                    .acquire(
                        GraphWarmLeaseIdentity {
                            project_id: "project-id".into(),
                            warm_request_id: format!("warm-{attempt}"),
                            graph_revision: "graph-revision".into(),
                        },
                        LeaseDeadlines {
                            queue_deadline_ms: 0,
                            launch_deadline_ms: 0,
                        },
                    )
                    .await,
                Err(djinn_k8s::GraphWarmLeaseError::Queued)
            ),
            "an unarmed cap queues every warm request instead of granting it"
        );
    }
    assert_eq!(
        repository.snapshot().await.unwrap().occupied,
        0,
        "nothing occupies capacity: the denials are the cap, not contention"
    );
}

#[tokio::test]
async fn an_unarmed_durable_cap_adopts_the_configured_build_slot_cap() {
    use djinn_db::AdmissionHandoffRepository;

    let (database, repository) = unarmed_production_database().await;
    let handoff = Arc::new(AdmissionHandoffRepository::new(database.clone()));
    // The fixed composition: the same configured cap the v0 journal controller
    // receives from DJINN_MAX_BUILD_TASKRUNS.
    let service = Arc::new(
        BuildLeaseService::new(Arc::clone(&repository), 3).with_handoff_epoch(handoff),
    );
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert_eq!(service.cap(), 3);

    let adapter = BuildLeaseGraphWarmAdapter::new(Arc::clone(&service));
    let grant = adapter
        .acquire(
            GraphWarmLeaseIdentity {
                project_id: "project-id".into(),
                warm_request_id: "warm-request".into(),
                graph_revision: "graph-revision".into(),
            },
            LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        )
        .await
        .expect("an armed cap authorizes the Job POST");
    assert_eq!(grant.identity.warm_request_id, "warm-request");
    assert_eq!(repository.snapshot().await.unwrap().occupied, 1);
    assert_eq!(
        repository.snapshot().await.unwrap().cap,
        3,
        "the first drain converges the durable table on the armed cap"
    );
}

#[tokio::test]
async fn re_seeding_the_handoff_row_alone_does_not_arm_the_lease() {
    use djinn_db::AdmissionHandoffRepository;

    let (database, repository) = unarmed_production_database().await;
    let handoff = Arc::new(AdmissionHandoffRepository::new(database.clone()));
    // The documented operator remedy for the deleted handoff row. The baseline
    // is seeded with a NULL cap, so it cannot by itself arm the lease: without
    // a configured fallback the chain still bottoms out at the durable 0.
    let seeded = handoff.seed_baseline().await.unwrap();
    assert_eq!(seeded.cap, None, "the seeded baseline carries no cap");

    let unconfigured = Arc::new(
        BuildLeaseService::new(Arc::clone(&repository), 0).with_handoff_epoch(Arc::clone(&handoff)),
    );
    assert!(matches!(unconfigured.recover().await, LeaseResult::Status(_)));
    assert_eq!(
        unconfigured.cap(),
        0,
        "seeding the handoff row is not sufficient to arm the lease"
    );

    let configured = Arc::new(
        BuildLeaseService::new(Arc::clone(&repository), 3).with_handoff_epoch(handoff),
    );
    assert!(matches!(configured.recover().await, LeaseResult::Status(_)));
    assert_eq!(
        configured.cap(),
        3,
        "a capless handoff row yields to the configured build-slot cap"
    );
}

#[tokio::test]
async fn an_armed_cap_is_never_overridden_by_the_configured_fallback() {
    use djinn_db::{AdmissionHandoffRepository, V0Mode, V1Mode};

    let (database, repository) = unarmed_production_database().await;
    // An operator-armed durable cap outranks the process configuration.
    repository.set_cap(1).await.unwrap();
    let armed = Arc::new(BuildLeaseService::new(Arc::clone(&repository), 9));
    assert!(matches!(armed.recover().await, LeaseResult::Status(_)));
    assert_eq!(
        armed.cap(),
        1,
        "a positive durable cap is authoritative over the configured fallback"
    );

    // And an armed handoff epoch cap outranks both.
    let handoff = Arc::new(AdmissionHandoffRepository::new(database.clone()));
    handoff
        .set_modes_and_cap(0, V0Mode::Enforce, V1Mode::Shadow, Some(5))
        .await
        .unwrap();
    let epoch_armed = Arc::new(
        BuildLeaseService::new(Arc::clone(&repository), 9).with_handoff_epoch(handoff),
    );
    assert!(matches!(epoch_armed.recover().await, LeaseResult::Status(_)));
    assert_eq!(epoch_armed.cap(), 5, "the epoch reference cap wins");
}
