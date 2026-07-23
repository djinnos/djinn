//! Deterministic restart fault matrix for durable graph-warm reconciliation.

use super::*;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

struct LeaseFake {
    rows: StdMutex<Vec<GraphWarmLeaseRecovery>>,
    binds: StdMutex<Vec<String>>,
    reports: StdMutex<Vec<LeaseState>>,
    releases: StdMutex<usize>,
}

#[async_trait]
impl GraphWarmLease for LeaseFake {
    async fn acquire(
        &self,
        _: GraphWarmLeaseIdentity,
        _: LeaseDeadlines,
    ) -> Result<GraphWarmLeaseGrant, GraphWarmLeaseError> {
        panic!("reconciliation must never acquire or create a fresh lease")
    }

    async fn bind(
        &self,
        _: &GraphWarmLeaseIdentity,
        _: LeaseFencingToken,
        uid: String,
    ) -> Result<(), GraphWarmLeaseError> {
        self.binds.lock().unwrap().push(uid);
        Ok(())
    }

    async fn report(
        &self,
        _: &GraphWarmLeaseIdentity,
        _: LeaseFencingToken,
        state: LeaseState,
    ) -> Result<(), GraphWarmLeaseError> {
        self.reports.lock().unwrap().push(state);
        Ok(())
    }

    async fn release(
        &self,
        identity: &GraphWarmLeaseIdentity,
        _: LeaseFencingToken,
    ) -> Result<(), GraphWarmLeaseError> {
        *self.releases.lock().unwrap() += 1;
        self.rows
            .lock()
            .unwrap()
            .retain(|row| row.identity != *identity);
        Ok(())
    }

    async fn recoverable(&self) -> Result<Vec<GraphWarmLeaseRecovery>, GraphWarmLeaseError> {
        Ok(self.rows.lock().unwrap().clone())
    }
}

struct CandidatesFake {
    inventories: StdMutex<VecDeque<WarmCandidateInventory>>,
    deletes: StdMutex<Vec<(String, Option<String>)>>,
    gates: StdMutex<Vec<String>>,
    delete_outcome: CleanupObservation,
}

#[async_trait]
impl WarmCandidateReconciler for CandidatesFake {
    async fn inventory(&self, _: &LeasedWarmJobIdentity) -> WarmCandidateInventory {
        let mut inventories = self.inventories.lock().unwrap();
        if inventories.len() > 1 {
            inventories.pop_front().unwrap()
        } else {
            inventories.front().cloned().unwrap_or_else(empty_inventory)
        }
    }

    async fn open_selected_pod_gate(
        &self,
        _: &LeasedWarmJobIdentity,
        inventory: &WarmCandidateInventory,
    ) -> GateObservation {
        self.gates.lock().unwrap().push(
            inventory
                .selected_pod()
                .and_then(|pod| pod.uid.clone())
                .unwrap_or_default(),
        );
        GateObservation::Opened
    }

    async fn delete_candidate(&self, candidate: &WarmCandidate) -> CleanupObservation {
        self.deletes
            .lock()
            .unwrap()
            .push((candidate.name.clone(), candidate.uid.clone()));
        self.delete_outcome.clone()
    }
}

fn recovery(
    state: LeaseState,
    bound_pod_uid: Option<&str>,
    expired: bool,
) -> GraphWarmLeaseRecovery {
    GraphWarmLeaseRecovery {
        identity: GraphWarmLeaseIdentity {
            project_id: "project".into(),
            warm_request_id: "request".into(),
            graph_revision: "revision".into(),
        },
        fencing_token: LeaseFencingToken(7),
        bound_pod_uid: bound_pod_uid.map(str::to_owned),
        state,
        deadlines: LeaseDeadlines {
            queue_deadline_ms: 0,
            launch_deadline_ms: if expired { 1 } else { 0 },
        },
        cleanup_required: false,
    }
}

fn empty_inventory() -> WarmCandidateInventory {
    WarmCandidateInventory {
        observation: WarmInventoryObservation::Observed,
        jobs_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
        jobs: WarmCandidateSet::default(),
        pods_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
        pods: WarmCandidateSet::default(),
    }
}

fn pod_inventory(uid: &str) -> WarmCandidateInventory {
    let candidate = WarmCandidate {
        kind: WarmCandidateKind::Pod,
        name: "pod".into(),
        uid: Some(uid.into()),
        annotation_validation: WarmAnnotationValidation::Matching,
    };
    WarmCandidateInventory {
        observation: WarmInventoryObservation::Observed,
        jobs_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
        jobs: WarmCandidateSet::default(),
        pods_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
        pods: WarmCandidateSet {
            state: WarmCandidateSetState::One,
            candidates: vec![candidate],
        },
    }
}

fn warmer(lease: Arc<LeaseFake>, candidates: Arc<CandidatesFake>) -> K8sGraphWarmer {
    struct Dispatcher;
    #[async_trait]
    impl WarmJobDispatcher for Dispatcher {
        async fn dispatch(&self, _: &str, _: Job) -> Result<String, String> {
            panic!("restart reconciliation must not create a Job")
        }
    }
    let mut warmer = K8sGraphWarmer::with_dispatcher(
        KubernetesConfig::for_testing(),
        Database::open_in_memory().unwrap(),
        Arc::new(Dispatcher),
        Arc::new(NoopJobWatcher),
    );
    warmer.dispatch.lease = Some(lease);
    warmer.dispatch.candidates = Some(candidates);
    warmer
}

#[tokio::test]
async fn restart_before_create_and_after_terminal_evidence_release_once() {
    let lease = Arc::new(LeaseFake {
        rows: StdMutex::new(vec![recovery(LeaseState::Launching, None, false)]),
        binds: StdMutex::new(Vec::new()),
        reports: StdMutex::new(Vec::new()),
        releases: StdMutex::new(0),
    });
    let candidates = Arc::new(CandidatesFake {
        inventories: StdMutex::new(VecDeque::from([empty_inventory()])),
        deletes: StdMutex::new(Vec::new()),
        gates: StdMutex::new(Vec::new()),
        delete_outcome: CleanupObservation::ConfirmedDelete,
    });
    let warmer = warmer(lease.clone(), candidates);
    warmer.reconcile_durable_warm_leases().await;
    warmer.reconcile_durable_warm_leases().await;
    assert_eq!(
        *lease.releases.lock().unwrap(),
        1,
        "terminal replay must not return capacity twice"
    );
    assert!(lease.binds.lock().unwrap().is_empty());
}

#[tokio::test]
async fn restart_after_ambiguous_create_before_after_bind_and_gate_open() {
    let lease = Arc::new(LeaseFake {
        rows: StdMutex::new(vec![
            recovery(LeaseState::Launching, None, false),
            recovery(LeaseState::Bound, Some("uid-a"), false),
            recovery(LeaseState::Active, Some("uid-a"), false),
        ]),
        binds: StdMutex::new(Vec::new()),
        reports: StdMutex::new(Vec::new()),
        releases: StdMutex::new(0),
    });
    let candidates = Arc::new(CandidatesFake {
        inventories: StdMutex::new(VecDeque::from([pod_inventory("uid-a")])),
        deletes: StdMutex::new(Vec::new()),
        gates: StdMutex::new(Vec::new()),
        delete_outcome: CleanupObservation::ConfirmedDelete,
    });
    let warmer = warmer(lease.clone(), candidates.clone());
    warmer.reconcile_durable_warm_leases().await;
    assert_eq!(
        lease.binds.lock().unwrap().as_slice(),
        ["uid-a", "uid-a", "uid-a"]
    );
    assert_eq!(
        candidates.gates.lock().unwrap().as_slice(),
        ["uid-a", "uid-a", "uid-a"]
    );
    assert_eq!(
        lease.reports.lock().unwrap().as_slice(),
        [LeaseState::Active, LeaseState::Active, LeaseState::Active]
    );
}

#[tokio::test]
async fn restart_from_each_durable_state_only_opens_one_confirmed_uid() {
    // Recovery uses one inventory/bind/gate sequence for every persisted state;
    // the fake dispatcher panics, so this also proves no restart creates work.
    for state in [
        LeaseState::Launching,
        LeaseState::Bound,
        LeaseState::Active,
        LeaseState::Suspect,
    ] {
        let lease = Arc::new(LeaseFake {
            rows: StdMutex::new(vec![recovery(state, Some("uid-a"), false)]),
            binds: StdMutex::new(Vec::new()),
            reports: StdMutex::new(Vec::new()),
            releases: StdMutex::new(0),
        });
        let candidates = Arc::new(CandidatesFake {
            inventories: StdMutex::new(VecDeque::from([pod_inventory("uid-a")])),
            deletes: StdMutex::new(Vec::new()),
            gates: StdMutex::new(Vec::new()),
            delete_outcome: CleanupObservation::ConfirmedDelete,
        });

        warmer(lease.clone(), candidates.clone())
            .reconcile_durable_warm_leases()
            .await;

        assert_eq!(lease.binds.lock().unwrap().as_slice(), ["uid-a"]);
        assert_eq!(candidates.gates.lock().unwrap().as_slice(), ["uid-a"]);
        assert_eq!(
            lease.reports.lock().unwrap().as_slice(),
            [LeaseState::Active]
        );
        assert_eq!(*lease.releases.lock().unwrap(), 0);
    }
}

#[tokio::test]
async fn unsafe_recovery_inventory_never_binds_or_opens_graph_work_gate() {
    let duplicate = {
        let mut first = pod_inventory("uid-a");
        let mut second = pod_inventory("uid-b");
        WarmCandidateInventory {
            observation: WarmInventoryObservation::Observed,
            jobs_observation:
                crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
            jobs: WarmCandidateSet::default(),
            pods_observation:
                crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
            pods: WarmCandidateSet {
                state: WarmCandidateSetState::Duplicate,
                candidates: vec![
                    first.pods.candidates.remove(0),
                    second.pods.candidates.remove(0),
                ],
            },
        }
    };
    let mismatch = WarmCandidateInventory {
        observation: WarmInventoryObservation::Observed,
        jobs_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
        jobs: WarmCandidateSet::default(),
        pods_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
        pods: WarmCandidateSet {
            state: WarmCandidateSetState::Unresolved,
            candidates: vec![WarmCandidate {
                kind: WarmCandidateKind::Pod,
                name: "mismatched-pod".into(),
                uid: Some("uid-a".into()),
                annotation_validation: WarmAnnotationValidation::Mismatch {
                    key: "djinn.app/graph-revision",
                    expected: "revision".into(),
                    found: Some("other".into()),
                },
            }],
        },
    };
    let api_unavailable = WarmCandidateInventory {
        observation: WarmInventoryObservation::ApiError("apiserver unavailable".into()),
        jobs_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::ApiError(
            "apiserver unavailable".into(),
        ),
        jobs: WarmCandidateSet::default(),
        pods_observation: crate::graph_warmer_candidates::WarmCandidateListObservation::Observed,
        pods: WarmCandidateSet::default(),
    };

    for inventory in [duplicate, mismatch, api_unavailable] {
        let lease = Arc::new(LeaseFake {
            rows: StdMutex::new(vec![recovery(LeaseState::Launching, None, false)]),
            binds: StdMutex::new(Vec::new()),
            reports: StdMutex::new(Vec::new()),
            releases: StdMutex::new(0),
        });
        let candidates = Arc::new(CandidatesFake {
            inventories: StdMutex::new(VecDeque::from([inventory])),
            deletes: StdMutex::new(Vec::new()),
            gates: StdMutex::new(Vec::new()),
            delete_outcome: CleanupObservation::ConfirmedDelete,
        });
        warmer(lease.clone(), candidates.clone())
            .reconcile_durable_warm_leases()
            .await;

        assert!(lease.binds.lock().unwrap().is_empty());
        assert!(candidates.gates.lock().unwrap().is_empty());
        assert_eq!(*lease.releases.lock().unwrap(), 0);
        assert_eq!(
            lease.reports.lock().unwrap().as_slice(),
            [LeaseState::Suspect]
        );
    }
}

#[tokio::test]
async fn restart_during_expired_deletion_keeps_suspect_until_absence_then_releases() {
    let lease = Arc::new(LeaseFake {
        rows: StdMutex::new(vec![recovery(LeaseState::Suspect, Some("uid-a"), true)]),
        binds: StdMutex::new(Vec::new()),
        reports: StdMutex::new(Vec::new()),
        releases: StdMutex::new(0),
    });
    let candidates = Arc::new(CandidatesFake {
        inventories: StdMutex::new(VecDeque::from([pod_inventory("uid-a"), empty_inventory()])),
        deletes: StdMutex::new(Vec::new()),
        gates: StdMutex::new(Vec::new()),
        delete_outcome: CleanupObservation::Unresolved("delete response lost".into()),
    });
    let warmer = warmer(lease.clone(), candidates.clone());
    warmer.reconcile_durable_warm_leases().await;
    assert_eq!(
        candidates.deletes.lock().unwrap().as_slice(),
        [("pod".into(), Some("uid-a".into()))]
    );
    assert_eq!(*lease.releases.lock().unwrap(), 0);
    assert_eq!(
        lease.reports.lock().unwrap().as_slice(),
        [LeaseState::Suspect]
    );
    warmer.reconcile_durable_warm_leases().await;
    assert_eq!(*lease.releases.lock().unwrap(), 1);
    assert!(
        candidates.gates.lock().unwrap().is_empty(),
        "expired work never opens a gate"
    );
}
