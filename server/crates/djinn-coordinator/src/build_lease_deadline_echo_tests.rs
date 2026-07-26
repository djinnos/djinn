//! The deadlines echoed back to a lease caller must be the deadlines that were
//! stored.
//!
//! These run against the real `BuildLeaseRepository` on a fresh database, so
//! they exercise the actual SQL projection rather than a hand-built row. The
//! defect they pin: the shared column list selected `queue_deadline::text`,
//! which renders PostgreSQL's own `2026-07-25 20:30:00+00` format, while
//! `build_lease::ms` parses RFC3339 and maps a parse failure to `0`. Because
//! `0` also means "no deadline" in this contract, every real deadline was
//! echoed as unbounded and nothing failed loudly.
//!
//! Expiry was never affected — it is evaluated in SQL against the stored column
//! — so only the echo path can catch this. Both halves of the contract are
//! asserted here: a real deadline echoes its own epoch milliseconds, and a row
//! stored without a deadline still echoes `0`.

use std::sync::Arc;

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseRow, Database,
};
use djinn_k8s::GraphWarmLease;
use djinn_supervisor::services::{
    GraphWarmLeaseIdentity, LeaseDeadlines, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest,
    LeaseResult, LeaseStatusRequest,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::build_lease::{
    BuildLeaseService, ManualLeaseClock, NoopLeaseTelemetry, NoopLeaseTransactionPause,
};
use crate::graph_warm_lease::BuildLeaseGraphWarmAdapter;

/// A fixed 2026 instant on a whole second, so the two deadlines below cover
/// both a sub-second and an exact-second rendering.
const NOW_MS: i64 = 1_785_312_000_000;
/// Deliberately carries a non-zero millisecond component.
const QUEUE_DEADLINE_MS: i64 = NOW_MS + 30_123;
/// Deliberately lands on an exact second, where PostgreSQL omits the fraction.
const LAUNCH_DEADLINE_MS: i64 = NOW_MS + 60_000;

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

async fn service(cap: i64) -> (Arc<BuildLeaseService>, Arc<BuildLeaseRepository>) {
    let repository = Arc::new(BuildLeaseRepository::new(
        Database::open_in_memory().unwrap(),
    ));
    let service = Arc::new(BuildLeaseService::with_seams(
        Arc::clone(&repository),
        cap,
        Arc::new(ManualLeaseClock::new(NOW_MS)),
        Arc::new(NoopLeaseTransactionPause),
        Arc::new(NoopLeaseTelemetry),
    ));
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert!(matches!(service.set_cap(cap).await, LeaseResult::Status(_)));
    (service, repository)
}

async fn durable_row(repository: &BuildLeaseRepository, id: &str) -> BuildLeaseRow {
    repository
        .get(&BuildLeaseKey {
            consumer_kind: BuildLeaseConsumerKind::GraphWarm,
            consumer_id: id.into(),
        })
        .await
        .expect("the durable row must be readable")
        .expect("the queued lease must exist")
}

fn parsed_ms(value: &str) -> i64 {
    (OffsetDateTime::parse(value, &Rfc3339)
        .unwrap_or_else(|error| panic!("durable deadline `{value}` is not RFC3339: {error}"))
        .unix_timestamp_nanos()
        / 1_000_000) as i64
}

/// A queued row echoes the deadlines it was stored with, through both the queue
/// response and a later status read.
#[tokio::test]
async fn queued_and_status_echo_the_stored_deadlines_rather_than_zero() {
    let (service, repository) = service(0).await;

    let LeaseResult::Queued(queued) = service
        .queue(request("echo", QUEUE_DEADLINE_MS, LAUNCH_DEADLINE_MS))
        .await
    else {
        panic!("a zero-cap queue must park the row as queued");
    };
    assert_eq!(
        (
            queued.deadlines.queue_deadline_ms,
            queued.deadlines.launch_deadline_ms
        ),
        (QUEUE_DEADLINE_MS, LAUNCH_DEADLINE_MS),
        "the queue response must echo the deadlines it stored, not 0"
    );

    let LeaseResult::Status(status) = service
        .status(LeaseStatusRequest {
            identity: warm("echo"),
        })
        .await
    else {
        panic!("a queued lease must report status");
    };
    assert_eq!(
        (
            status.deadlines.queue_deadline_ms,
            status.deadlines.launch_deadline_ms
        ),
        (QUEUE_DEADLINE_MS, LAUNCH_DEADLINE_MS),
        "a status read must echo the durable deadlines, not 0"
    );

    // The durable strings themselves are the canonical representation, so the
    // one parser the coordinator owns round-trips them without loss.
    let row = durable_row(&repository, "echo").await;
    let queue_deadline = row.queue_deadline.expect("the queued row retains it");
    let launch_deadline = row.launch_deadline.expect("the queued row retains it");
    assert_eq!(parsed_ms(&queue_deadline), QUEUE_DEADLINE_MS);
    assert_eq!(parsed_ms(&launch_deadline), LAUNCH_DEADLINE_MS);
}

/// The grant a caller actually launches on carries the same deadlines.
#[tokio::test]
async fn a_grant_echoes_the_stored_deadlines_rather_than_zero() {
    let (service, _) = service(1).await;

    let LeaseResult::Granted(grant) = service
        .queue(request("granted", QUEUE_DEADLINE_MS, LAUNCH_DEADLINE_MS))
        .await
    else {
        panic!("a lease queued under free capacity must grant");
    };
    assert_eq!(
        (
            grant.deadlines.queue_deadline_ms,
            grant.deadlines.launch_deadline_ms
        ),
        (QUEUE_DEADLINE_MS, LAUNCH_DEADLINE_MS),
        "the grant must echo the deadlines it stored, not 0"
    );
}

/// The other half of the contract: `0` genuinely means "no deadline", the
/// column stays NULL, and the echo must keep saying `0`.
#[tokio::test]
async fn an_absent_deadline_still_echoes_zero_and_stays_null() {
    let (service, repository) = service(1).await;

    let LeaseResult::Granted(grant) = service.queue(request("unbounded", 0, 0)).await else {
        panic!("a lease queued under free capacity must grant");
    };
    assert_eq!(
        (
            grant.deadlines.queue_deadline_ms,
            grant.deadlines.launch_deadline_ms
        ),
        (0, 0),
        "an unbounded lease must keep echoing 0"
    );

    let row = durable_row(&repository, "unbounded").await;
    assert_eq!(row.queue_deadline, None);
    assert_eq!(row.launch_deadline, None);
}

/// Warm recovery is a live consumer that was already reading `0`.
///
/// `BuildLeaseGraphWarmAdapter::recoverable` parses the durable launch deadline
/// with the same RFC3339 parser, so under the defect every warm lease recovered
/// across a coordinator restart came back unbounded.
#[tokio::test]
async fn warm_recovery_reports_the_stored_launch_deadline() {
    let (service, _) = service(1).await;
    let LeaseResult::Granted(grant) = service
        .queue(request("recovered", QUEUE_DEADLINE_MS, LAUNCH_DEADLINE_MS))
        .await
    else {
        panic!("a lease queued under free capacity must grant");
    };
    // Only launching/bound/active/suspect rows are recoverable, so acknowledge
    // the grant before recovering.
    assert!(matches!(
        service
            .grant(LeaseGrantRequest {
                identity: warm("recovered"),
                fencing_token: grant.fencing_token,
            })
            .await,
        LeaseResult::Status(_)
    ));

    let adapter = BuildLeaseGraphWarmAdapter::new(Arc::clone(&service));
    let recovered = adapter.recoverable().await.expect("recovery must succeed");
    let entry = recovered
        .iter()
        .find(|entry| entry.identity.warm_request_id == "recovered")
        .expect("the launching warm lease must be recovered");
    assert_eq!(
        entry.deadlines.launch_deadline_ms, LAUNCH_DEADLINE_MS,
        "warm recovery must report the durable launch deadline, not 0"
    );
}
