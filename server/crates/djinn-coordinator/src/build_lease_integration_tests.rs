//! Deterministic v1 lease state-machine coverage.
//!
//! These tests use the injected lease clock and isolated durable ledger; no
//! wall-clock sleeps participate in grant, deadline, or recovery ordering.

use std::sync::Arc;

use djinn_db::{BuildLeaseRepository, BuildLeaseState, Database};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseBindRequest, LeaseDeadlines, LeaseIdentity, LeaseQueueRequest,
    LeaseReleaseRequest, LeaseResult,
};

use crate::build_lease::{BuildLeaseService, ManualLeaseClock};

fn warm(id: &str) -> LeaseIdentity {
    LeaseIdentity::GraphWarm(GraphWarmLeaseIdentity {
        project_id: "project-id".into(),
        warm_request_id: id.into(),
        graph_revision: "graph-revision".into(),
    })
}

fn request(id: &str, queue_deadline_ms: i64, launch_deadline_ms: i64) -> LeaseQueueRequest {
    LeaseQueueRequest {
        identity: warm(id),
        deadlines: LeaseDeadlines {
            queue_deadline_ms,
            launch_deadline_ms,
        },
    }
}

async fn service(
    cap: i64,
) -> (
    Arc<BuildLeaseService>,
    Arc<BuildLeaseRepository>,
    Arc<ManualLeaseClock>,
) {
    let repository = Arc::new(BuildLeaseRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let clock = Arc::new(ManualLeaseClock::new(100));
    let service = Arc::new(BuildLeaseService::with_seams(
        Arc::clone(&repository),
        cap,
        clock.clone(),
        Arc::new(crate::build_lease::NoopLeaseTransactionPause),
        Arc::new(crate::build_lease::NoopLeaseTelemetry),
    ));
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert!(matches!(service.set_cap(cap).await, LeaseResult::Status(_)));
    (service, repository, clock)
}

#[tokio::test]
async fn unavailable_precedes_recovery_but_committed_deadline_returns_one_credit() {
    let repository = Arc::new(BuildLeaseRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let not_recovered = BuildLeaseService::new(repository, 1);
    assert!(matches!(
        not_recovered.queue(request("unavailable", 0, 0)).await,
        LeaseResult::LeaseUnavailable
    ));

    let (service, repository, clock) = service(0).await;
    assert!(matches!(
        service.queue(request("deadline", 110, 0)).await,
        LeaseResult::Queued(_)
    ));
    clock.set_ms(110);
    service.expire_deadlines().await;

    let first = service.queue(request("deadline", 110, 0)).await;
    assert!(matches!(
        first,
        LeaseResult::LeaseWaitTimeout {
            timeout_credit: Some(credit)
        } if credit.units == 1 && credit.retry_after_ms == 0
    ));
    assert!(matches!(
        service.queue(request("deadline", 110, 0)).await,
        LeaseResult::LeaseWaitTimeout {
            timeout_credit: None
        }
    ));
    let row = repository
        .get(&djinn_db::BuildLeaseKey {
            consumer_kind: djinn_db::BuildLeaseConsumerKind::GraphWarm,
            consumer_id: "deadline".into(),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, BuildLeaseState::Terminal);
    assert_eq!(row.terminal_reason.as_deref(), Some("deadline_expired"));
    assert!(row.timeout_credit_consumed);
}

#[tokio::test]
async fn stable_warm_bind_launch_deadline_and_restart_preserve_occupancy() {
    let (service, repository, clock) = service(1).await;
    let granted = service.queue(request("stable-warm-request", 0, 110)).await;
    let token = match granted {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("expected grant, got {other:?}"),
    };
    assert!(matches!(
        service
            .bind(LeaseBindRequest {
                identity: warm("stable-warm-request"),
                fencing_token: token.clone(),
                pod_uid: "immutable-pod-uid".into(),
            })
            .await,
        LeaseResult::Bound(_)
    ));
    // The same candidate is idempotent; another pod UID cannot replace it.
    assert!(matches!(
        service
            .bind(LeaseBindRequest {
                identity: warm("stable-warm-request"),
                fencing_token: token,
                pod_uid: "different-candidate".into(),
            })
            .await,
        LeaseResult::LeaseUnavailable
    ));
    clock.set_ms(110);
    service.expire_deadlines().await;
    let recovered = BuildLeaseService::with_seams(
        Arc::clone(&repository),
        0,
        clock,
        Arc::new(crate::build_lease::NoopLeaseTransactionPause),
        Arc::new(crate::build_lease::NoopLeaseTelemetry),
    );
    assert!(matches!(recovered.recover().await, LeaseResult::Status(_)));
    let snapshot = repository.snapshot().await.unwrap();
    assert_eq!(snapshot.occupied, 1);
    assert_eq!(
        snapshot.rows[0].bound_pod_uid.as_deref(),
        Some("immutable-pod-uid")
    );
}

#[tokio::test]
async fn duplicate_release_returns_capacity_once_and_drains_fifo() {
    let (service, repository, _) = service(1).await;
    let first = service.queue(request("first", 0, 0)).await;
    let token = match first {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("expected first grant, got {other:?}"),
    };
    assert!(matches!(
        service.queue(request("second", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    let release = LeaseReleaseRequest {
        identity: warm("first"),
        fencing_token: token,
        candidate_cleanup: true,
    };
    assert!(matches!(
        service.release(release.clone()).await,
        LeaseResult::Released {
            candidate_cleanup: true
        }
    ));
    assert!(matches!(
        service.release(release).await,
        LeaseResult::Released {
            candidate_cleanup: true
        }
    ));
    let snapshot = repository.snapshot().await.unwrap();
    assert_eq!(snapshot.occupied, 1);
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].key.consumer_id, "second");
    assert_eq!(snapshot.rows[0].state, BuildLeaseState::Granted);
}
