//! The lifecycle ledger must record a warm Job even when the v1 lease granted it.
//!
//! Split out of `graph_warmer_tests.rs`, which sits close to the 51200-byte
//! `Server Guards` ceiling. Shares that module's warmer harness rather than
//! duplicating it.

use super::tests::{
    AdmissionRecording, LifecycleRecordingDispatcher, UidWatcher, seed_project_with_ready_image,
    test_config,
};
use super::*;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// A lease that always grants, standing in for an armed v1 build lease.
struct AlwaysGrantingLease;

#[async_trait]
impl crate::GraphWarmLease for AlwaysGrantingLease {
    async fn acquire(
        &self,
        identity: djinn_supervisor::services::GraphWarmLeaseIdentity,
        _deadlines: djinn_supervisor::services::LeaseDeadlines,
    ) -> Result<crate::GraphWarmLeaseGrant, crate::GraphWarmLeaseError> {
        Ok(crate::GraphWarmLeaseGrant {
            identity,
            grant: djinn_supervisor::services::LeaseGrant {
                fencing_token: LeaseFencingToken(7),
                deadlines: djinn_supervisor::services::LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
            },
        })
    }

    async fn bind(
        &self,
        _identity: &djinn_supervisor::services::GraphWarmLeaseIdentity,
        _fencing_token: LeaseFencingToken,
        _pod_uid: String,
    ) -> Result<(), crate::GraphWarmLeaseError> {
        Ok(())
    }
}

/// A lease-granted warm Job must STILL be recorded in the lifecycle ledger.
///
/// This drives the warmer itself, not just the controller, because the defect
/// lived in the warmer's own match: `(Some(lease_grant), _) => None` meant that
/// once the v1 lease was armed -- which is always, in production, whenever the
/// cap is positive -- a warm Job wrote no admission-journal row at all. The
/// journal is what records the Kubernetes UID and what #2597's
/// `reclaim_absent_object` reads to retire occupancy whose object is provably
/// gone, so a leased warm Job was invisible to reclamation.
///
/// The lease still governs whether the Job may be created; the ledger append is
/// capacity-neutral and simply must also happen.
#[tokio::test]
async fn a_lease_granted_warm_job_is_still_recorded_in_the_lifecycle_ledger() {
    let db = Database::open_in_memory().expect("in-memory db");
    let project_id = seed_project_with_ready_image(&db, "proj-leased-ledger").await;
    let events = Arc::new(Mutex::new(Vec::new()));
    // The leased path polls `wait_for_bind_and_open_leased_candidate` for
    // `warm_job_timeout_seconds` awaiting a Kubernetes candidate this harness
    // deliberately does not provide (it stubs the dispatcher, not the candidate
    // inventory). The default 3600s would hang the test. One second is enough:
    // the ledger writes this test asserts on -- the reservation and
    // CreateStarted -- both happen BEFORE the POST and therefore before that
    // wait is ever entered, so shortening the deadline changes nothing about
    // what is being proven, it just lets the trigger return.
    let mut config = test_config();
    config.warm_job_timeout_seconds = 1;
    let warmer = K8sGraphWarmer::with_dispatcher(
        config,
        db,
        Arc::new(LifecycleRecordingDispatcher {
            events: events.clone(),
            result: Ok("leased-warm".to_string()),
        }),
        Arc::new(UidWatcher),
    )
    .with_graph_warm_lease(Arc::new(AlwaysGrantingLease))
    .with_warm_admission(Arc::new(AdmissionRecording {
        events: events.clone(),
        admit_error: None,
        fail_create_started: false,
    }));

    warmer.trigger(&project_id).await;
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    let events = events.lock().await;
    assert!(
        events.iter().any(|event| event.starts_with("reserve:")),
        "a lease-granted warm Job wrote no ledger row: {events:?} \
         -- it is invisible to absent-object reclamation"
    );
    assert!(
        events.iter().any(|event| event == "CreateStarted"),
        "the ledger row must reach CreateStarted before the POST: {events:?}"
    );
}
