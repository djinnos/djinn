//! Deterministic v1 lease state-machine coverage.
//!
//! These tests use the injected lease clock and isolated durable ledger; no
//! wall-clock sleeps participate in grant, deadline, or recovery ordering.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_db::{BuildLeaseRepository, BuildLeaseState, Database};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseAbandonRequest, LeaseBindRequest, LeaseCancelRequest,
    LeaseDeadlines, LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest, LeaseResult,
};
use tokio::sync::{Semaphore, mpsc};

use crate::build_lease::{
    BuildLeaseService, LeaseOperation, LeaseTransactionPause, ManualLeaseClock,
};

/// A test-only gate that selects transaction serialization without wall time.
struct TransactionGate {
    operation: LeaseOperation,
    arrived: mpsc::UnboundedSender<LeaseOperation>,
    permits: Semaphore,
}
impl TransactionGate {
    fn new(operation: LeaseOperation) -> (Arc<Self>, mpsc::UnboundedReceiver<LeaseOperation>) {
        let (arrived, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                operation,
                arrived,
                permits: Semaphore::new(0),
            }),
            receiver,
        )
    }
    fn release(&self) {
        self.permits.add_permits(1);
    }
}
#[async_trait]
impl LeaseTransactionPause for TransactionGate {
    async fn before_transaction(&self, operation: LeaseOperation) {
        if operation == self.operation {
            let _ = self.arrived.send(operation);
            self.permits.acquire().await.unwrap().forget();
        }
    }
}

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
                fencing_token: token.clone(),
                pod_uid: "immutable-pod-uid".into(),
            })
            .await,
        LeaseResult::Bound(_)
    ));
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

async fn contenders(
    cap: i64,
    operation: LeaseOperation,
) -> (
    Arc<BuildLeaseService>,
    Arc<BuildLeaseService>,
    Arc<BuildLeaseRepository>,
    Arc<ManualLeaseClock>,
    Arc<TransactionGate>,
    mpsc::UnboundedReceiver<LeaseOperation>,
) {
    let (seed, repository, clock) = service(cap).await;
    let (gate, arrived) = TransactionGate::new(operation);
    let make = || {
        Arc::new(BuildLeaseService::with_seams(
            repository.clone(),
            cap,
            clock.clone(),
            gate.clone(),
            Arc::new(crate::build_lease::NoopLeaseTelemetry),
        ))
    };
    let left = make();
    let right = make();
    drop(seed);
    assert!(matches!(left.recover().await, LeaseResult::Status(_)));
    assert!(matches!(right.recover().await, LeaseResult::Status(_)));
    (left, right, repository, clock, gate, arrived)
}

#[tokio::test]
async fn paused_abandon_and_grant_cover_both_serialization_orders() {
    let (abandoner, allocator, repository, _, gate, mut arrived) =
        contenders(0, LeaseOperation::Abandon).await;
    assert!(matches!(
        abandoner.queue(request("grant-wins", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    let pending = tokio::spawn({
        let s = abandoner.clone();
        async move {
            s.abandon(LeaseAbandonRequest {
                identity: warm("grant-wins"),
                candidate_cleanup: false,
            })
            .await
        }
    });
    assert_eq!(arrived.recv().await, Some(LeaseOperation::Abandon));
    assert!(matches!(allocator.set_cap(1).await, LeaseResult::Status(_)));
    gate.release();
    assert!(matches!(
        pending.await.unwrap(),
        LeaseResult::LeaseUnavailable
    ));
    assert_eq!(
        repository.snapshot().await.unwrap().rows[0].state,
        BuildLeaseState::Granted
    );

    let (abandoner, allocator, repository, _, gate, mut arrived) =
        contenders(0, LeaseOperation::SetCap).await;
    assert!(matches!(
        abandoner.queue(request("abandon-wins", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    let pending = tokio::spawn({
        let s = allocator.clone();
        async move { s.set_cap(1).await }
    });
    assert_eq!(arrived.recv().await, Some(LeaseOperation::SetCap));
    assert!(matches!(
        abandoner
            .abandon(LeaseAbandonRequest {
                identity: warm("abandon-wins"),
                candidate_cleanup: true
            })
            .await,
        LeaseResult::Abandoned {
            candidate_cleanup: true
        }
    ));
    gate.release();
    let _ = pending.await.unwrap();
    assert!(repository.snapshot().await.unwrap().rows.is_empty());
}

#[tokio::test]
async fn paused_same_instant_deadline_and_grant_cover_both_orders() {
    let (expirer, allocator, repository, clock, gate, mut arrived) =
        contenders(0, LeaseOperation::Expire).await;
    assert!(matches!(
        expirer.queue(request("grant-first", 110, 0)).await,
        LeaseResult::Queued(_)
    ));
    clock.set_ms(110);
    let pending = tokio::spawn({
        let s = expirer.clone();
        async move { s.expire_deadlines().await }
    });
    assert_eq!(arrived.recv().await, Some(LeaseOperation::Expire));
    assert!(matches!(allocator.set_cap(1).await, LeaseResult::Status(_)));
    gate.release();
    let _ = pending.await.unwrap();
    // At the exact instant `<= deadline` wins even when set-cap serialized
    // first: grant_next durably expires before selecting a candidate.
    assert_eq!(repository.snapshot().await.unwrap().occupied, 0);
    assert!(matches!(
        expirer.queue(request("grant-first", 110, 0)).await,
        LeaseResult::LeaseWaitTimeout { .. }
    ));

    let (expirer, allocator, repository, clock, gate, mut arrived) =
        contenders(0, LeaseOperation::SetCap).await;
    assert!(matches!(
        expirer.queue(request("deadline-first", 110, 0)).await,
        LeaseResult::Queued(_)
    ));
    clock.set_ms(110);
    let pending = tokio::spawn({
        let s = allocator.clone();
        async move { s.set_cap(1).await }
    });
    assert_eq!(arrived.recv().await, Some(LeaseOperation::SetCap));
    assert!(matches!(
        expirer.expire_deadlines().await,
        LeaseResult::Status(_)
    ));
    gate.release();
    let _ = pending.await.unwrap();
    assert!(matches!(
        expirer.queue(request("deadline-first", 110, 0)).await,
        LeaseResult::LeaseWaitTimeout { .. }
    ));
    assert_eq!(repository.snapshot().await.unwrap().occupied, 0);
}

#[tokio::test]
async fn lost_terminal_responses_cleanup_and_suspect_occupancy_retry() {
    let (service, repository, clock) = service(1).await;
    let token = match service.queue(request("warm-candidate", 0, 110)).await {
        LeaseResult::Granted(g) => g.fencing_token,
        other => panic!("expected grant, got {other:?}"),
    };
    assert!(matches!(
        service.queue(request("warm-candidate", 0, 110)).await,
        LeaseResult::Granted(_)
    ));
    clock.set_ms(110);
    let _ = service.expire_deadlines().await;
    assert_eq!(
        repository.snapshot().await.unwrap().rows[0].state,
        BuildLeaseState::Suspect
    );
    let cancel = LeaseCancelRequest {
        identity: warm("warm-candidate"),
        fencing_token: Some(token),
        candidate_cleanup: true,
    };
    assert!(matches!(
        service.cancel(cancel.clone()).await,
        LeaseResult::Cancelled {
            candidate_cleanup: true
        }
    ));
    assert!(matches!(
        service.cancel(cancel).await,
        LeaseResult::Cancelled {
            candidate_cleanup: true
        }
    ));
    assert_eq!(repository.snapshot().await.unwrap().occupied, 0);
}
