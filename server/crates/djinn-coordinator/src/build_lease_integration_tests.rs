//! Deterministic v1 lease state-machine coverage.
//!
//! These tests use the injected lease clock and isolated durable ledger; no
//! wall-clock sleeps participate in grant, deadline, or recovery ordering.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_db::{BuildLeaseRepository, BuildLeaseState, Database};
use djinn_k8s::GraphWarmLease;
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseAbandonRequest, LeaseBindRequest, LeaseCancelRequest,
    LeaseDeadlines, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest,
    LeaseResult, LeaseState, LeaseStatusRequest, TaskInvocationLeaseIdentity,
};
use tokio::sync::{Semaphore, mpsc};

use crate::build_lease::{
    BuildLeaseService, LeaseOperation, LeaseTelemetry, LeaseTelemetryConsumer,
    LeaseTelemetryOutcome, LeaseTelemetryState, LeaseTransactionPause, ManualLeaseClock,
};
use crate::graph_warm_lease::BuildLeaseGraphWarmAdapter;

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
    // Eagerly initialize exactly one isolated database before constructing
    // either contender. A throwaway service previously coupled contender
    // recovery to lazy test-database setup and teardown ownership.
    let database = Database::open_in_memory().unwrap();
    database.ensure_initialized().await.unwrap();
    let repository = Arc::new(BuildLeaseRepository::new(database));
    repository.set_cap(cap).await.unwrap();
    let clock = Arc::new(ManualLeaseClock::new(100));
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
    let snapshot = repository.snapshot().await.unwrap();
    assert_eq!(snapshot.occupied, 1);
    assert_eq!(snapshot.rows[0].state, BuildLeaseState::Suspect);
    assert_eq!(
        snapshot.rows[0]
            .candidate_cleanup
            .as_ref()
            .and_then(|value| value.get("close_requested"))
            .and_then(|value| value.as_str()),
        Some("cancelled")
    );
}

#[tokio::test]
async fn production_adapter_persists_reports_and_replays_forward_bind() {
    let (service, repository, _) = service(4).await;
    for (index, replay_state) in [LeaseState::Bound, LeaseState::Active, LeaseState::Suspect]
        .into_iter()
        .enumerate()
    {
        let identity = GraphWarmLeaseIdentity {
            project_id: "project-id".into(),
            warm_request_id: format!("adapter-{index}"),
            graph_revision: "graph-revision".into(),
        };
        let adapter = BuildLeaseGraphWarmAdapter::new(service.clone());
        let token = adapter
            .acquire(
                identity.clone(),
                LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            )
            .await
            .unwrap()
            .grant
            .fencing_token;
        adapter
            .report(&identity, token.clone(), LeaseState::Launching)
            .await
            .unwrap();
        adapter
            .bind(&identity, token.clone(), "uid-a".into())
            .await
            .unwrap();
        adapter
            .report(&identity, token.clone(), LeaseState::Bound)
            .await
            .unwrap();
        if matches!(replay_state, LeaseState::Active | LeaseState::Suspect) {
            adapter
                .report(&identity, token.clone(), LeaseState::Active)
                .await
                .unwrap();
        }
        if replay_state == LeaseState::Suspect {
            adapter
                .report(&identity, token.clone(), LeaseState::Suspect)
                .await
                .unwrap();
        }
        adapter
            .bind(&identity, token.clone(), "uid-a".into())
            .await
            .unwrap();
        let row = repository
            .get(&djinn_db::BuildLeaseKey {
                consumer_kind: djinn_db::BuildLeaseConsumerKind::GraphWarm,
                consumer_id: identity.warm_request_id.clone(),
            })
            .await
            .unwrap()
            .unwrap();
        let expected = match replay_state {
            LeaseState::Bound => BuildLeaseState::Bound,
            LeaseState::Active => BuildLeaseState::Active,
            LeaseState::Suspect => BuildLeaseState::Suspect,
            _ => unreachable!(),
        };
        assert_eq!(row.state, expected);
        assert_eq!(row.bound_pod_uid.as_deref(), Some("uid-a"));
        assert!(
            adapter
                .report(
                    &identity,
                    djinn_supervisor::services::LeaseFencingToken(token.0 + 1),
                    replay_state,
                )
                .await
                .is_err()
        );
        assert!(
            adapter
                .report(&identity, token, LeaseState::Launching)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn lost_grant_and_abandon_responses_replay_durable_winners() {
    let (grant_service, _, _) = service(1).await;
    let token = match grant_service.queue(request("lost-grant", 0, 0)).await {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("expected grant, got {other:?}"),
    };
    let grant = LeaseGrantRequest {
        identity: warm("lost-grant"),
        fencing_token: token,
    };
    assert!(
        matches!(grant_service.grant(grant.clone()).await, LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Launching)
    );
    assert!(
        matches!(grant_service.grant(grant).await, LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Launching)
    );

    let (service, _, _) = service(0).await;
    assert!(matches!(
        service.queue(request("lost-abandon", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    let abandon = LeaseAbandonRequest {
        identity: warm("lost-abandon"),
        candidate_cleanup: true,
    };
    assert!(matches!(
        service.abandon(abandon.clone()).await,
        LeaseResult::Abandoned {
            candidate_cleanup: true
        }
    ));
    assert!(matches!(
        service.abandon(abandon).await,
        LeaseResult::Abandoned {
            candidate_cleanup: true
        }
    ));
}

#[tokio::test]
async fn queued_restart_and_warm_status_are_idempotent() {
    let (service, repository, clock) = service(0).await;
    assert!(matches!(
        service.queue(request("queued-after-restart", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    let recovered = BuildLeaseService::with_seams(
        repository,
        0,
        clock,
        Arc::new(crate::build_lease::NoopLeaseTransactionPause),
        Arc::new(crate::build_lease::NoopLeaseTelemetry),
    );
    assert!(matches!(recovered.recover().await, LeaseResult::Status(_)));
    assert!(
        matches!(recovered.status(LeaseStatusRequest { identity: warm("queued-after-restart") }).await, LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Queued)
    );
    assert!(
        matches!(recovered.status(LeaseStatusRequest { identity: warm("queued-after-restart") }).await, LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Queued)
    );
}

#[tokio::test]
async fn graph_warm_and_task_invocation_share_one_fifo_cap() {
    let (service, _, _) = service(1).await;
    let warm_identity = warm("fifo-warm");
    let task_identity = LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "invocation".into(),
    });
    let warm_token = match service
        .queue(LeaseQueueRequest {
            identity: warm_identity.clone(),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        })
        .await
    {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("expected warm grant, got {other:?}"),
    };
    assert!(matches!(
        service
            .queue(LeaseQueueRequest {
                identity: task_identity.clone(),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0
                },
            })
            .await,
        LeaseResult::Queued(_)
    ));
    assert!(matches!(
        service
            .release(LeaseReleaseRequest {
                identity: warm_identity,
                fencing_token: warm_token,
                candidate_cleanup: true
            })
            .await,
        LeaseResult::Released { .. }
    ));
    assert!(matches!(
        service
            .queue(LeaseQueueRequest {
                identity: task_identity,
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0
                },
            })
            .await,
        LeaseResult::Granted(_)
    ));
}

#[tokio::test]
async fn adapter_recovers_lost_bind_response_from_durable_status_and_rejects_other_uid() {
    let (service, _, _) = service(1).await;
    let identity = GraphWarmLeaseIdentity {
        project_id: "project-id".into(),
        warm_request_id: "lost-bind-response".into(),
        graph_revision: "graph-revision".into(),
    };
    let adapter = BuildLeaseGraphWarmAdapter::new(service.clone());
    let token = adapter
        .acquire(
            identity.clone(),
            LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        )
        .await
        .unwrap()
        .grant
        .fencing_token;

    // Simulate a committed bind whose response was lost, followed by normal
    // activation before the retry arrives. The adapter's bind call cannot get a
    // fresh Bound result, so it must confirm the durable same-UID status.
    assert!(matches!(
        service
            .bind(LeaseBindRequest {
                identity: LeaseIdentity::GraphWarm(identity.clone()),
                fencing_token: token.clone(),
                pod_uid: "immutable-uid".into(),
            })
            .await,
        LeaseResult::Bound(_)
    ));
    assert!(matches!(
        service
            .report(
                LeaseIdentity::GraphWarm(identity.clone()),
                token.clone(),
                LeaseState::Active,
            )
            .await,
        LeaseResult::Status(_)
    ));
    assert!(
        adapter
            .bind(&identity, token.clone(), "immutable-uid".into())
            .await
            .is_ok()
    );
    assert!(
        adapter
            .bind(&identity, token, "different-uid".into())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn task_then_warm_fifo_and_zero_cap_do_not_bypass_the_shared_ledger() {
    let (lease_service, repository, _) = service(1).await;
    let task = LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: "task".into(),
        task_run_id: "run".into(),
        invocation_id: "first".into(),
    });
    let task_token = match lease_service
        .queue(LeaseQueueRequest {
            identity: task.clone(),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        })
        .await
    {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("expected task grant, got {other:?}"),
    };
    assert!(matches!(
        lease_service.queue(request("warm-after-task", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    assert!(matches!(
        lease_service
            .release(LeaseReleaseRequest {
                identity: task,
                fencing_token: task_token,
                candidate_cleanup: true,
            })
            .await,
        LeaseResult::Released { .. }
    ));
    assert!(matches!(
        lease_service.queue(request("warm-after-task", 0, 0)).await,
        LeaseResult::Granted(_)
    ));
    assert_eq!(repository.snapshot().await.unwrap().occupied, 1);

    let (zero_cap, zero_repository, _) = service(0).await;
    assert!(matches!(
        zero_cap.queue(request("warm-cap-zero", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    assert!(matches!(
        zero_cap
            .queue(LeaseQueueRequest {
                identity: LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
                    task_id: "task".into(),
                    task_run_id: "run".into(),
                    invocation_id: "cap-zero".into(),
                }),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0
                },
            })
            .await,
        LeaseResult::Queued(_)
    ));
    let snapshot = zero_repository.snapshot().await.unwrap();
    assert_eq!(snapshot.occupied, 0);
    assert_eq!(snapshot.rows.len(), 2);
    assert!(
        snapshot
            .rows
            .iter()
            .all(|row| row.state == BuildLeaseState::Queued)
    );
}

#[derive(Clone)]
struct RecoveryCandidateClient {
    pods: Arc<std::sync::Mutex<Vec<djinn_k8s::WarmCandidateObject>>>,
    deleted: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    gates: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl djinn_k8s::WarmCandidateClient for RecoveryCandidateClient {
    async fn list_warm_jobs(&self) -> Result<Vec<djinn_k8s::WarmCandidateObject>, String> {
        Ok(Vec::new())
    }

    async fn list_warm_pods(&self) -> Result<Vec<djinn_k8s::WarmCandidateObject>, String> {
        Ok(self.pods.lock().unwrap().clone())
    }

    async fn open_gate(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &djinn_k8s::LeasedWarmJobIdentity,
    ) -> djinn_k8s::GateObservation {
        self.gates.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        djinn_k8s::GateObservation::Opened
    }

    async fn delete_uid(
        &self,
        candidate: &djinn_k8s::WarmCandidate,
    ) -> djinn_k8s::CleanupObservation {
        let Some(uid) = candidate.uid.clone() else {
            return djinn_k8s::CleanupObservation::Unresolved("missing UID".into());
        };
        self.deleted
            .lock()
            .unwrap()
            .push((candidate.name.clone(), uid.clone()));
        self.pods
            .lock()
            .unwrap()
            .retain(|pod| pod.uid.as_deref() != Some(uid.as_str()));
        djinn_k8s::CleanupObservation::ConfirmedDelete
    }
}

struct RecoveryDispatcher;
#[async_trait]
impl djinn_k8s::WarmJobDispatcher for RecoveryDispatcher {
    async fn dispatch(&self, _: &str, _: djinn_k8s::WarmJobManifest) -> Result<String, String> {
        panic!("restart reconciliation must not create a Job")
    }
}

#[tokio::test]
async fn production_adapter_cancellation_reconciles_uid_before_capacity_release() {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (service, repository, _) = service(1).await;
    let identity = GraphWarmLeaseIdentity {
        project_id: "project-id".into(),
        warm_request_id: "cancel-recovery".into(),
        graph_revision: "graph-revision".into(),
    };
    let adapter = Arc::new(BuildLeaseGraphWarmAdapter::new(service.clone()));
    let token = adapter
        .acquire(
            identity.clone(),
            LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        )
        .await
        .unwrap()
        .grant
        .fencing_token;
    adapter
        .bind(&identity, token.clone(), "pod-uid".into())
        .await
        .unwrap();
    adapter
        .report(&identity, token.clone(), LeaseState::Active)
        .await
        .unwrap();
    assert!(matches!(
        service
            .cancel(LeaseCancelRequest {
                identity: LeaseIdentity::GraphWarm(identity.clone()),
                fencing_token: Some(token.clone()),
                candidate_cleanup: true,
            })
            .await,
        LeaseResult::Cancelled { .. }
    ));

    let pods = Arc::new(std::sync::Mutex::new(vec![
        djinn_k8s::WarmCandidateObject {
            kind: djinn_k8s::WarmCandidateKind::Pod,
            name: "pod-a".into(),
            uid: Some("pod-uid".into()),
            annotations: BTreeMap::from([
                (
                    "djinn.app/warm-request-id".into(),
                    identity.warm_request_id.clone(),
                ),
                (
                    "djinn.app/graph-revision".into(),
                    identity.graph_revision.clone(),
                ),
                ("djinn.app/fencing-token".into(), token.0.to_string()),
            ]),
        },
    ]));
    let deleted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let gates = Arc::new(AtomicUsize::new(0));
    let client = RecoveryCandidateClient {
        pods,
        deleted: deleted.clone(),
        gates: gates.clone(),
    };
    let warmer = djinn_k8s::K8sGraphWarmer::with_dispatcher(
        djinn_k8s::KubernetesConfig::for_testing(),
        Database::open_in_memory().unwrap(),
        Arc::new(RecoveryDispatcher),
        Arc::new(djinn_k8s::NoopJobWatcher),
    )
    .with_graph_warm_lease(adapter)
    .with_warm_candidate_client(client);

    warmer.reconcile_durable_warm_leases().await;
    assert_eq!(repository.snapshot().await.unwrap().occupied, 1);
    assert_eq!(
        deleted.lock().unwrap().as_slice(),
        [("pod-a".into(), "pod-uid".into())]
    );
    assert_eq!(gates.load(Ordering::SeqCst), 0);

    warmer.reconcile_durable_warm_leases().await;
    assert_eq!(repository.snapshot().await.unwrap().occupied, 0);
    warmer.reconcile_durable_warm_leases().await;
    assert_eq!(repository.snapshot().await.unwrap().occupied, 0);
    assert_eq!(deleted.lock().unwrap().len(), 1);
}

/// AC3 (v1 restart): recovery reads the durable admission epoch and its
/// reference cap before the lease service opens. A committed epoch with a cap is
/// observed; a deleted (missing/unreadable) epoch row fails closed to no
/// observed epoch.
#[tokio::test]
async fn recovery_reads_admission_epoch_and_reference_cap() {
    use djinn_db::{AdmissionHandoffRepository, V0Mode, V1Mode};

    let database = Database::open_in_memory().unwrap();
    let handoff = Arc::new(AdmissionHandoffRepository::new(database.clone()));
    // Advance the durable epoch and set a reference cap of 7.
    handoff
        .set_modes_and_cap(0, V0Mode::Enforce, V1Mode::Shadow, Some(7))
        .await
        .unwrap();
    let current = handoff.read().await.unwrap().unwrap();
    assert_eq!(current.epoch, 1);
    assert_eq!(current.cap, Some(7));

    let repository = Arc::new(BuildLeaseRepository::new(database.clone()));
    let service = Arc::new(
        BuildLeaseService::new(Arc::clone(&repository), 1).with_handoff_epoch(Arc::clone(&handoff)),
    );
    // Before recovery the epoch has not been observed.
    assert_eq!(service.observed_epoch(), None);
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    // Recovery observed the current epoch and adopted its reference cap.
    assert_eq!(service.observed_epoch(), Some(1));

    // A missing epoch row fails closed: the observed epoch is cleared.
    handoff.delete_for_test().await.unwrap();
    let restarted = Arc::new(
        BuildLeaseService::new(Arc::new(BuildLeaseRepository::new(database.clone())), 1)
            .with_handoff_epoch(Arc::clone(&handoff)),
    );
    assert!(matches!(restarted.recover().await, LeaseResult::Status(_)));
    assert_eq!(
        restarted.observed_epoch(),
        None,
        "a missing epoch row must leave the observed epoch unknown (fail closed)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// u2oz — end-to-end cross-component invariant proving suite (cap-3 / cap-zero).
//
// The tests above prove each lease behaviour in isolation. The ones below prove
// the WHOLE stack together under a single cap: task-invocation and graph-warm
// consumers interleave on the one FIFO, the hard invariant (occupied never
// exceeds the cap) holds across saturation and FIFO promotion, a restart reloads
// occupied + queued rows AND the admission epoch before it admits again, and
// every telemetry label the authority emits is a bounded enum (never a raw id).
// ─────────────────────────────────────────────────────────────────────────────

/// Bounded-label recording telemetry: it can only ever receive the closed
/// enum vocabulary (`LeaseOperation`, `LeaseTelemetryConsumer`,
/// `LeaseTelemetryOutcome`, `LeaseTelemetryState`) plus integer counts. There is
/// no seam through which a task id, warm-request id, or pod UID could reach a
/// metric label — that is the structural guarantee AC4 asserts, and these
/// captures let the tests confirm the emitted vocabulary stays inside the domain.
#[derive(Default)]
struct RecordingLeaseTelemetry {
    operations: std::sync::Mutex<Vec<(LeaseOperation, LeaseTelemetryConsumer)>>,
    outcomes: std::sync::Mutex<Vec<LeaseTelemetryOutcome>>,
    states: std::sync::Mutex<Vec<(LeaseTelemetryState, u64)>>,
}
impl RecordingLeaseTelemetry {
    fn recorded_consumer(&self, expected: LeaseTelemetryConsumer) -> bool {
        self.operations
            .lock()
            .unwrap()
            .iter()
            .any(|(_, consumer)| *consumer == expected)
    }
    fn recorded_outcome(&self, expected: LeaseTelemetryOutcome) -> bool {
        self.outcomes.lock().unwrap().contains(&expected)
    }
}
impl LeaseTelemetry for RecordingLeaseTelemetry {
    fn state(&self, state: LeaseTelemetryState, count: u64) {
        self.states.lock().unwrap().push((state, count));
    }
    fn outcome(&self, outcome: LeaseTelemetryOutcome) {
        self.outcomes.lock().unwrap().push(outcome);
    }
    fn operation(&self, operation: LeaseOperation, consumer: LeaseTelemetryConsumer) {
        self.operations.lock().unwrap().push((operation, consumer));
    }
}

async fn service_with_telemetry(
    cap: i64,
    telemetry: Arc<dyn LeaseTelemetry>,
) -> (Arc<BuildLeaseService>, Arc<BuildLeaseRepository>) {
    let repository = Arc::new(BuildLeaseRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let service = Arc::new(BuildLeaseService::with_seams(
        Arc::clone(&repository),
        cap,
        Arc::new(ManualLeaseClock::new(100)),
        Arc::new(crate::build_lease::NoopLeaseTransactionPause),
        telemetry,
    ));
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert!(matches!(service.set_cap(cap).await, LeaseResult::Status(_)));
    (service, repository)
}

fn task(invocation: &str) -> LeaseIdentity {
    LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: "cap-three-task".into(),
        task_run_id: "cap-three-run".into(),
        invocation_id: invocation.into(),
    })
}

/// AC1 + AC4: at cap 3, escalating task trees and graph-warm consumers share the
/// one FIFO. Occupancy never exceeds 3 across saturation, and each release
/// promotes exactly the oldest queued consumer (mixed task/warm FIFO). Every
/// telemetry label emitted throughout is a bounded enum drawn from the closed
/// vocabulary, and both consumer kinds plus the system operations are exercised.
#[tokio::test]
async fn cap_three_mixed_consumers_hold_invariant_and_release_promotes_fifo() {
    let telemetry = Arc::new(RecordingLeaseTelemetry::default());
    let (service, repository) = service_with_telemetry(3, telemetry.clone()).await;

    async fn occupied(repository: &BuildLeaseRepository) -> i64 {
        repository.snapshot().await.unwrap().occupied
    }
    async fn queued(repository: &BuildLeaseRepository) -> usize {
        repository
            .snapshot()
            .await
            .unwrap()
            .rows
            .iter()
            .filter(|row| row.state == BuildLeaseState::Queued)
            .count()
    }

    // Fill the three units with a warm / task / warm interleave.
    let warm_a = match service.queue(request("warm-a", 0, 0)).await {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("warm A must grant, got {other:?}"),
    };
    let task_b = match service
        .queue(LeaseQueueRequest {
            identity: task("task-b"),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        })
        .await
    {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("task B must grant, got {other:?}"),
    };
    let _warm_c = match service.queue(request("warm-c", 0, 0)).await {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("warm C must grant, got {other:?}"),
    };
    assert_eq!(occupied(&repository).await, 3, "three units fill the cap");

    // The fourth (task) and fifth (warm) escalations queue behind the cap.
    assert!(matches!(
        service
            .queue(LeaseQueueRequest {
                identity: task("task-d"),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0
                },
            })
            .await,
        LeaseResult::Queued(_)
    ));
    assert!(matches!(
        service.queue(request("warm-e", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    assert_eq!(
        occupied(&repository).await,
        3,
        "queued escalations never breach the cap"
    );
    assert_eq!(queued(&repository).await, 2);

    // Releasing warm A promotes exactly the oldest queued consumer (task D):
    // occupancy is conserved at 3, and the FIFO ordering is honoured across
    // consumer kinds — warm E stays queued behind task D.
    assert!(matches!(
        service
            .release(LeaseReleaseRequest {
                identity: warm("warm-a"),
                fencing_token: warm_a,
                candidate_cleanup: true,
            })
            .await,
        LeaseResult::Released {
            candidate_cleanup: true
        }
    ));
    assert_eq!(occupied(&repository).await, 3, "release promotes one unit");
    assert!(
        matches!(
            service.status(LeaseStatusRequest { identity: task("task-d") }).await,
            LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Granted
        ),
        "task D (oldest queued) is promoted first"
    );
    assert!(
        matches!(
            service.status(LeaseStatusRequest { identity: warm("warm-e") }).await,
            LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Queued
        ),
        "warm E stays queued behind task D"
    );

    // Releasing task B promotes warm E; occupancy again holds at 3.
    assert!(matches!(
        service
            .release(LeaseReleaseRequest {
                identity: task("task-b"),
                fencing_token: task_b,
                candidate_cleanup: true,
            })
            .await,
        LeaseResult::Released { .. }
    ));
    assert_eq!(occupied(&repository).await, 3);
    assert!(
        matches!(
            service.status(LeaseStatusRequest { identity: warm("warm-e") }).await,
            LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Granted
        ),
        "warm E promotes once a second unit frees"
    );

    // AC4 (bounded labels): both consumer kinds and the system operations were
    // recorded, and the captured domains are exactly the closed enum vocabulary.
    assert!(telemetry.recorded_consumer(LeaseTelemetryConsumer::TaskInvocation));
    assert!(telemetry.recorded_consumer(LeaseTelemetryConsumer::GraphWarm));
    assert!(telemetry.recorded_consumer(LeaseTelemetryConsumer::System));
    assert!(
        telemetry.recorded_outcome(LeaseTelemetryOutcome::Granted),
        "grants are recorded as a bounded outcome"
    );
    // The occupancy gauge (published on recover) never reports above the cap.
    for (state, count) in telemetry.states.lock().unwrap().iter() {
        if *state == LeaseTelemetryState::Occupied {
            assert!(*count <= 3, "occupied gauge label never exceeds the cap");
        }
    }
}

/// AC4 (v1 restart): a fresh coordinator generation reloads occupied AND queued
/// rows and the durable admission epoch (adopting its authoritative reference
/// cap) BEFORE it admits again. The restarted authority is handed a deliberately
/// wrong local cap of 0; it must converge on the epoch's cap of 3, conserve the
/// three reloaded occupied units, keep the two reloaded queued rows, and refuse
/// to admit beyond the reloaded occupancy.
#[tokio::test]
async fn cap_three_restart_reloads_occupied_queued_and_epoch_before_admission() {
    use djinn_db::{AdmissionHandoffRepository, V0Mode, V1Mode};

    let database = Database::open_in_memory().unwrap();
    database.ensure_initialized().await.unwrap();
    let handoff = Arc::new(AdmissionHandoffRepository::new(database.clone()));
    // Commit an epoch carrying the authoritative reference cap of 3.
    handoff
        .set_modes_and_cap(0, V0Mode::Enforce, V1Mode::Shadow, Some(3))
        .await
        .unwrap();
    let epoch = handoff.read().await.unwrap().unwrap().epoch;

    let repository = Arc::new(BuildLeaseRepository::new(database.clone()));
    let first =
        BuildLeaseService::new(Arc::clone(&repository), 3).with_handoff_epoch(Arc::clone(&handoff));
    assert!(matches!(first.recover().await, LeaseResult::Status(_)));

    // Occupy three units (warm / warm / task) and queue two more.
    let warm_a = match first.queue(request("warm-a", 0, 0)).await {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("expected grant, got {other:?}"),
    };
    assert!(matches!(
        first.queue(request("warm-b", 0, 0)).await,
        LeaseResult::Granted(_)
    ));
    assert!(matches!(
        first
            .queue(LeaseQueueRequest {
                identity: task("task-c"),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0
                },
            })
            .await,
        LeaseResult::Granted(_)
    ));
    assert!(matches!(
        first
            .queue(LeaseQueueRequest {
                identity: task("task-d"),
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0
                },
            })
            .await,
        LeaseResult::Queued(_)
    ));
    assert!(matches!(
        first.queue(request("warm-e", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    assert_eq!(repository.snapshot().await.unwrap().occupied, 3);

    // Restart: a brand-new generation over the same durable rows + epoch, handed
    // the wrong local cap of 0.
    let restarted =
        BuildLeaseService::new(Arc::clone(&repository), 0).with_handoff_epoch(Arc::clone(&handoff));
    assert_eq!(
        restarted.observed_epoch(),
        None,
        "the epoch is unobserved before recovery"
    );
    assert!(matches!(restarted.recover().await, LeaseResult::Status(_)));
    assert_eq!(
        restarted.observed_epoch(),
        Some(epoch),
        "recovery observes the durable epoch before admitting"
    );

    let recovered = restarted.recovery_snapshot().await.unwrap();
    assert_eq!(recovered.occupied, 3, "reloads the three occupied units");
    assert_eq!(
        recovered
            .rows
            .iter()
            .filter(|row| row.state == BuildLeaseState::Queued)
            .count(),
        2,
        "reloads both queued rows"
    );
    assert_eq!(
        recovered.cap, 3,
        "adopts the epoch reference cap, not the wrong local cap"
    );

    // Admission after restart is bounded by the reloaded occupancy: a fresh
    // queue stays queued behind the three reloaded units.
    assert!(matches!(
        restarted.queue(request("warm-after-restart", 0, 0)).await,
        LeaseResult::Queued(_)
    ));
    assert_eq!(restarted.recovery_snapshot().await.unwrap().occupied, 3);

    // Releasing one reloaded unit promotes the oldest reloaded queued row (task
    // D) under the adopted cap of 3 — proving the epoch cap, not the local 0,
    // governs post-restart draining.
    assert!(matches!(
        restarted
            .release(LeaseReleaseRequest {
                identity: warm("warm-a"),
                fencing_token: warm_a,
                candidate_cleanup: true,
            })
            .await,
        LeaseResult::Released { .. }
    ));
    assert_eq!(restarted.recovery_snapshot().await.unwrap().occupied, 3);
    assert!(
        matches!(
            restarted.status(LeaseStatusRequest { identity: task("task-d") }).await,
            LeaseResult::Status(status) if status.state == djinn_supervisor::services::LeaseState::Granted
        ),
        "the adopted cap of 3 drains the oldest reloaded queued row"
    );
}
