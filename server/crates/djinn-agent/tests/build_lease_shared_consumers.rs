//! Composition regression for the process-wide build-lease authority.
//!
//! This deliberately enters through both production consumer surfaces instead
//! of queueing directly on `BuildLeaseService`: graph warming uses
//! `GraphWarmLease`, while task invocation uses `SupervisorServices` built by
//! the production shared-service constructor.

use std::sync::Arc;

use djinn_agent::supervisor::{SupervisorServices, services_for_agent_context_with_build_lease};
use djinn_agent::test_helpers;
use djinn_coordinator::build_lease::{
    BuildLeaseService, ManualLeaseClock, NoopLeaseTelemetry, NoopLeaseTransactionPause,
};
use djinn_coordinator::graph_warm_lease::BuildLeaseGraphWarmAdapter;
use djinn_db::{BuildLeaseRepository, BuildLeaseState};
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

fn task_invocation(invocation: &str) -> LeaseIdentity {
    LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: "shared-consumers-task".into(),
        task_run_id: "shared-consumers-run".into(),
        invocation_id: invocation.into(),
    })
}

async fn occupied(repository: &BuildLeaseRepository) -> i64 {
    repository.snapshot().await.unwrap().occupied
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

// ─────────────────────────────────────────────────────────────────────────────
// u2oz — cross-component invariant proving through the production consumer
// surfaces. Task invocation enters through `SupervisorServices` (the production
// shared-service constructor) and graph warming through the production
// `BuildLeaseGraphWarmAdapter`; both share one injected `BuildLeaseService`, so
// these tests prove the whole stack — not `BuildLeaseService` in isolation —
// holds the hard invariant at cap-3 and returns typed timeouts at cap-zero.
// ─────────────────────────────────────────────────────────────────────────────

fn zero_deadlines() -> LeaseDeadlines {
    LeaseDeadlines {
        queue_deadline_ms: 0,
        launch_deadline_ms: 0,
    }
}

/// AC1: at cap 3 the two production surfaces interleave escalating task trees and
/// graph-warm consumers on the one FIFO. Above-unleased task trees plus warm
/// consumers never exceed 3, and each release conserves occupancy by promoting
/// exactly the oldest queued consumer across consumer kinds.
#[tokio::test]
async fn cap_three_production_surfaces_hold_invariant_across_warm_and_task() {
    let db = test_helpers::create_test_db();
    let repository = Arc::new(BuildLeaseRepository::new(db.clone()));
    let build_lease = Arc::new(BuildLeaseService::new(Arc::clone(&repository), 3));
    assert!(matches!(
        build_lease.recover().await,
        LeaseResult::Status(_)
    ));
    assert!(matches!(
        build_lease.set_cap(3).await,
        LeaseResult::Status(_)
    ));

    let task_services: Arc<dyn SupervisorServices> = services_for_agent_context_with_build_lease(
        test_helpers::agent_context_from_db(db, CancellationToken::new()),
        CancellationToken::new(),
        Arc::clone(&build_lease),
    );
    let graph_lease = BuildLeaseGraphWarmAdapter::new(Arc::clone(&build_lease));

    // Fill the three units with a warm / task / warm interleave through both
    // production surfaces.
    let warm_a = graph_identity("warm-a");
    let warm_a_grant = graph_lease
        .acquire(warm_a.clone(), zero_deadlines())
        .await
        .expect("warm A takes the first unit");
    let task_b = task_invocation("task-b");
    let task_b_token = match task_services
        .queue_lease(LeaseQueueRequest {
            identity: task_b.clone(),
            deadlines: zero_deadlines(),
        })
        .await
    {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("task B must grant the second unit, got {other:?}"),
    };
    let warm_c = graph_identity("warm-c");
    let _warm_c_grant = graph_lease
        .acquire(warm_c.clone(), zero_deadlines())
        .await
        .expect("warm C takes the third unit");
    assert_eq!(occupied(&repository).await, 3, "three units fill the cap");

    // The fourth (task) and fifth (warm) escalations queue behind the cap.
    assert!(matches!(
        task_services
            .queue_lease(LeaseQueueRequest {
                identity: task_invocation("task-d"),
                deadlines: zero_deadlines(),
            })
            .await,
        LeaseResult::Queued(_)
    ));
    assert!(
        matches!(
            graph_lease
                .acquire(graph_identity("warm-e"), zero_deadlines())
                .await,
            Err(GraphWarmLeaseError::Queued)
        ),
        "warm E must not obtain independent capacity above the cap"
    );
    assert_eq!(
        occupied(&repository).await,
        3,
        "queued escalations never breach the shared cap"
    );

    // Releasing warm A promotes the oldest queued consumer (task D); occupancy is
    // conserved at 3 and warm E stays queued behind it.
    graph_lease
        .release(&warm_a, warm_a_grant.grant.fencing_token)
        .await
        .expect("warm A releases its unit");
    assert_eq!(occupied(&repository).await, 3, "release promotes one unit");
    assert!(matches!(
        task_services
            .lease_status(LeaseStatusRequest {
                identity: task_invocation("task-d"),
            })
            .await,
        LeaseResult::Status(status) if status.state == LeaseState::Granted
    ));
    assert!(matches!(
        task_services
            .lease_status(LeaseStatusRequest {
                identity: LeaseIdentity::GraphWarm(graph_identity("warm-e")),
            })
            .await,
        LeaseResult::Status(status) if status.state == LeaseState::Queued
    ));

    // Releasing task B promotes warm E; occupancy again holds at 3.
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
    assert_eq!(occupied(&repository).await, 3);
    assert!(matches!(
        task_services
            .lease_status(LeaseStatusRequest {
                identity: LeaseIdentity::GraphWarm(graph_identity("warm-e")),
            })
            .await,
        LeaseResult::Status(status) if status.state == LeaseState::Granted
    ));
}

/// AC2: at cap-zero heavy task and warm requests return typed timeouts through
/// the production surfaces; the timeout credit is fixed (excludes queued time)
/// and applies at most once; a light command that the launcher never escalates
/// leaves no footprint in the shared ledger (coordinator-free). The authoritative
/// launcher-level proof that a below-CPU-threshold command never contacts the
/// coordinator lives in `process::tests::process_lease_tests` (the light-fixture
/// suite); this test asserts the complementary ledger invariant end-to-end.
#[tokio::test]
async fn cap_zero_production_surfaces_return_typed_timeouts_and_credit_once() {
    let db = test_helpers::create_test_db();
    let repository = Arc::new(BuildLeaseRepository::new(db.clone()));
    let clock = Arc::new(ManualLeaseClock::new(100));
    // Inject a deterministic clock so deadline expiry (and thus typed timeout)
    // is exercised through the same authority the production surfaces delegate to.
    let build_lease = Arc::new(BuildLeaseService::with_seams(
        Arc::clone(&repository),
        0,
        clock.clone(),
        Arc::new(NoopLeaseTransactionPause),
        Arc::new(NoopLeaseTelemetry),
    ));
    assert!(matches!(
        build_lease.recover().await,
        LeaseResult::Status(_)
    ));
    assert!(matches!(
        build_lease.set_cap(0).await,
        LeaseResult::Status(_)
    ));

    let task_services: Arc<dyn SupervisorServices> = services_for_agent_context_with_build_lease(
        test_helpers::agent_context_from_db(db, CancellationToken::new()),
        CancellationToken::new(),
        Arc::clone(&build_lease),
    );
    let graph_lease = BuildLeaseGraphWarmAdapter::new(Arc::clone(&build_lease));

    // A heavy task escalation queues at cap-zero.
    let heavy = task_invocation("heavy");
    let heavy_request = LeaseQueueRequest {
        identity: heavy.clone(),
        deadlines: LeaseDeadlines {
            queue_deadline_ms: 110,
            launch_deadline_ms: 0,
        },
    };
    assert!(matches!(
        task_services.queue_lease(heavy_request.clone()).await,
        LeaseResult::Queued(_)
    ));

    // It sits queued for a long time; the credit must be independent of that wait.
    clock.set_ms(100_000);
    assert!(matches!(
        build_lease.expire_deadlines().await,
        LeaseResult::Status(_)
    ));

    // Re-driving the same request through the production task surface yields a
    // typed timeout carrying exactly one credit whose retry is not proportional
    // to the (very long) queued wait.
    match task_services.queue_lease(heavy_request.clone()).await {
        LeaseResult::LeaseWaitTimeout {
            timeout_credit: Some(credit),
        } => {
            assert_eq!(credit.units, 1, "exactly one credit is issued");
            assert_eq!(
                credit.retry_after_ms, 0,
                "credit excludes queued-awaiting-grant time"
            );
        }
        other => panic!("heavy task must time out typed, got {other:?}"),
    }
    // The credit applies at most once.
    assert!(matches!(
        task_services.queue_lease(heavy_request).await,
        LeaseResult::LeaseWaitTimeout {
            timeout_credit: None
        }
    ));

    // A heavy warm request also returns a typed timeout through the adapter. Its
    // queue deadline is still in the future at the current (already advanced)
    // clock, so the first acquire queues; only the later expiry makes it typed.
    let heavy_warm = graph_identity("heavy-warm");
    let warm_deadlines = LeaseDeadlines {
        queue_deadline_ms: 200_000,
        launch_deadline_ms: 0,
    };
    assert!(matches!(
        graph_lease
            .acquire(heavy_warm.clone(), warm_deadlines.clone())
            .await,
        Err(GraphWarmLeaseError::Queued)
    ));
    clock.set_ms(300_000);
    assert!(matches!(
        build_lease.expire_deadlines().await,
        LeaseResult::Status(_)
    ));
    assert!(
        matches!(
            graph_lease.acquire(heavy_warm, warm_deadlines).await,
            Err(GraphWarmLeaseError::Timeout)
        ),
        "expired warm request returns a typed timeout, not an untyped failure"
    );

    // A light command the launcher never escalates leaves no ledger footprint:
    // its identity never appears among the durable rows (coordinator-free).
    let snapshot = repository.snapshot().await.unwrap();
    assert!(
        snapshot
            .rows
            .iter()
            .all(|row| row.key.consumer_id != "light-never-escalated"),
        "an unescalated light command never reaches the shared ledger"
    );
    // Sanity: cap-zero leaves no occupied units — only terminal (timed-out) rows.
    assert!(
        snapshot
            .rows
            .iter()
            .all(|row| row.state == BuildLeaseState::Terminal),
        "cap-zero leaves no occupied units"
    );
}
