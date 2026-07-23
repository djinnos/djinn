//! Composition regression for the process-wide build-lease authority.
//!
//! This deliberately enters through both production consumer surfaces instead
//! of queueing directly on `BuildLeaseService`: graph warming uses
//! `GraphWarmLease`, while task invocation uses `SupervisorServices` built by
//! the production shared-service constructor.

use std::sync::Arc;

use djinn_agent::supervisor::{SupervisorServices, services_for_agent_context_with_build_lease};
use djinn_agent::test_helpers;
use djinn_coordinator::build_lease::BuildLeaseService;
use djinn_coordinator::graph_warm_lease::BuildLeaseGraphWarmAdapter;
use djinn_db::BuildLeaseRepository;
use djinn_k8s::{GraphWarmLease, GraphWarmLeaseError};
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseDeadlines, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest,
    LeaseReleaseRequest, LeaseResult, LeaseState, LeaseStatusRequest, TaskInvocationLeaseIdentity,
};
use tokio_util::sync::CancellationToken;

fn deadlines() -> LeaseDeadlines {
    LeaseDeadlines {
        queue_deadline_ms: 0,
        launch_deadline_ms: 0,
    }
}

fn graph_identity(request: &str) -> GraphWarmLeaseIdentity {
    GraphWarmLeaseIdentity {
        project_id: "shared-consumers-project".into(),
        warm_request_id: request.into(),
        graph_revision: "shared-consumers-revision".into(),
    }
}

fn task_identity() -> LeaseIdentity {
    LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: "task-b".into(),
        task_run_id: "run-b".into(),
        invocation_id: "invocation-b".into(),
    })
}

#[tokio::test]
async fn production_consumers_share_one_strict_fifo_and_one_unit_cap() {
    let db = test_helpers::create_test_db();
    let build_lease = Arc::new(BuildLeaseService::new(
        Arc::new(BuildLeaseRepository::new(db.clone())),
        1,
    ));
    assert!(matches!(
        build_lease.recover().await,
        LeaseResult::Status(_)
    ));
    assert!(matches!(
        build_lease.set_cap(1).await,
        LeaseResult::Status(_)
    ));

    // These are the same production composition surfaces used by AppState:
    // both receive this exact Arc rather than constructing their own authority.
    let task_services: Arc<dyn SupervisorServices> = services_for_agent_context_with_build_lease(
        test_helpers::agent_context_from_db(db, CancellationToken::new()),
        CancellationToken::new(),
        Arc::clone(&build_lease),
    );
    let graph_lease = BuildLeaseGraphWarmAdapter::new(Arc::clone(&build_lease));

    // Mixed FIFO order: graph A, task B, graph C.
    let graph_a = graph_identity("graph-a");
    let graph_a_grant = graph_lease
        .acquire(graph_a.clone(), deadlines())
        .await
        .expect("graph A must acquire the sole unit");
    let task_b = task_identity();
    assert!(matches!(
        task_services
            .queue_lease(LeaseQueueRequest {
                identity: task_b.clone(),
                deadlines: deadlines(),
            })
            .await,
        LeaseResult::Queued(status) if status.state == LeaseState::Queued
    ));
    let graph_c = graph_identity("graph-c");
    assert!(
        matches!(
            graph_lease.acquire(graph_c.clone(), deadlines()).await,
            Err(GraphWarmLeaseError::Queued)
        ),
        "graph C must not obtain independent graph-warm capacity"
    );

    assert!(matches!(
        task_services
            .lease_status(LeaseStatusRequest {
                identity: LeaseIdentity::GraphWarm(graph_a.clone()),
            })
            .await,
        LeaseResult::Status(status) if status.state == LeaseState::Launching
    ));
    assert!(matches!(
        task_services
            .lease_status(LeaseStatusRequest {
                identity: task_b.clone(),
            })
            .await,
        LeaseResult::Status(status) if status.state == LeaseState::Queued
    ));
    assert!(matches!(
        task_services
            .lease_status(LeaseStatusRequest {
                identity: LeaseIdentity::GraphWarm(graph_c.clone()),
            })
            .await,
        LeaseResult::Status(status) if status.state == LeaseState::Queued
    ));

    assert!(matches!(
        task_services
            .release_lease(LeaseReleaseRequest {
                identity: LeaseIdentity::GraphWarm(graph_a),
                fencing_token: graph_a_grant.grant.fencing_token,
                candidate_cleanup: true,
            })
            .await,
        LeaseResult::Released { .. }
    ));

    // Replaying B through the task surface observes its FIFO promotion. C must
    // remain queued while B owns the sole counted unit.
    let task_b_token = match task_services
        .queue_lease(LeaseQueueRequest {
            identity: task_b.clone(),
            deadlines: deadlines(),
        })
        .await
    {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("task B must be promoted before graph C, got {other:?}"),
    };
    assert!(
        matches!(
            graph_lease.acquire(graph_c.clone(), deadlines()).await,
            Err(GraphWarmLeaseError::Queued)
        ),
        "graph C must remain queued while task B holds the unit"
    );
    assert!(matches!(
        task_services
            .grant_lease(LeaseGrantRequest {
                identity: task_b.clone(),
                fencing_token: task_b_token.clone(),
            })
            .await,
        LeaseResult::Status(status) if status.state == LeaseState::Launching
    ));
    assert!(matches!(
        task_services
            .release_lease(LeaseReleaseRequest {
                identity: task_b,
                fencing_token: task_b_token,
                candidate_cleanup: true,
            })
            .await,
        LeaseResult::Released { .. }
    ));

    let graph_c_grant = graph_lease
        .acquire(graph_c.clone(), deadlines())
        .await
        .expect("graph C must acquire only after task B releases");
    assert!(matches!(
        task_services
            .lease_status(LeaseStatusRequest {
                identity: LeaseIdentity::GraphWarm(graph_c),
            })
            .await,
        LeaseResult::Status(status)
            if status.state == LeaseState::Launching
                && status.fencing_token == Some(graph_c_grant.grant.fencing_token)
    ));
}
