//! Owned, serde-safe v1 lease protocol contracts.
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long anything may hold a position in the ONE build-lease FIFO before the
/// coordinator terminalizes it as `deadline_expired`.
///
/// # Why one constant for two populations
///
/// [`grant_next`](../../../../djinn_db/repositories/build_lease) reads only the
/// queue HEAD and grants nothing when the head does not fit. Head-of-line
/// blocking is deliberate (it is what keeps a heavy warm Job from starving
/// behind an unbroken stream of weight-1 dispatches), and its cost is that
/// every queued row waits for the row in front of it — across consumer kinds.
///
/// So the two deadlines are not independent knobs. A dispatch row sits in the
/// FIFO for up to this long; an invocation queued behind it therefore has to be
/// willing to wait at least as long, or it expires first, degrades to the
/// launcher's 250m unleased quota, and — because the degrade is one-way — runs
/// its whole compile at ~1/16th of the leased quota.
///
/// Production ran with 30 MINUTES for dispatch and 30 SECONDS for invocations.
/// Denied dispatch attempts re-queued every ~30s for hours (279 denials in 3h),
/// so there was almost always a weight-1 dispatch row ahead of every zero-weight
/// invocation, and the invocation's own deadline always expired first: two live
/// task-run pods launched 7 and 5 lease invocations and ZERO escalated, while a
/// sampled leaf held `cpu.max = 25000 100000` after burning 52.8 CPU-seconds
/// (211x the 0.25 CPU-s escalation threshold).
///
/// Both consumers derive their deadline from this value so the two can no
/// longer drift apart. Waiting longer is free for the invocation: a queued
/// invocation's child keeps running (clamped) the whole time, and the command's
/// own timeout still bounds it — the only thing the queue deadline decides is
/// whether the escalation is still allowed to land when the FIFO reaches it.
pub const BUILD_LEASE_QUEUE_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// [`BUILD_LEASE_QUEUE_DEADLINE`] in whole milliseconds, for the coordinator's
/// epoch-millisecond deadline arithmetic.
pub const BUILD_LEASE_QUEUE_DEADLINE_MS: i64 = 30 * 60 * 1000;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod queue_deadline_tests {
    use super::*;

    /// The millisecond projection and the `Duration` are one value expressed
    /// two ways; a hand-edited drift between them would silently reinstate the
    /// mismatch this constant exists to remove.
    #[test]
    fn millisecond_projection_matches_the_duration() {
        assert_eq!(
            BUILD_LEASE_QUEUE_DEADLINE_MS,
            i64::try_from(BUILD_LEASE_QUEUE_DEADLINE.as_millis()).unwrap()
        );
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInvocationLeaseIdentity {
    pub task_id: String,
    pub task_run_id: String,
    pub invocation_id: String,
}
/// Layer-1 dispatch admission identity: one task-run attempt.
///
/// Deliberately generation-scoped rather than task-scoped. A generation is one
/// object lifecycle (one create, at most one Kubernetes UID, one release), and
/// the journal -- not the caller -- decides which generation an attempt gets
/// (`AdmissionJournalRepository::resolve_dispatch_generation`). Keying the
/// dispatch slot by the RESOLVED generation is what makes a retry of the same
/// attempt idempotent while a genuinely new attempt buys its own capacity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDispatchLeaseIdentity {
    pub task_id: String,
    pub generation: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphWarmLeaseIdentity {
    pub project_id: String,
    pub warm_request_id: String,
    pub graph_revision: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// Every population that can occupy a build slot.
///
/// All three share ONE cap over ONE FIFO. Before this, dispatch admission was
/// accounted in a separate table against the same configured number, so the two
/// authorities together admitted twice the operator's intent.
pub enum LeaseIdentity {
    /// Layer 2: measured, role-agnostic per-invocation escalation.
    TaskInvocation(TaskInvocationLeaseIdentity),
    /// Layer 1: coarse, role-derived pre-spawn reservation.
    TaskDispatch(TaskDispatchLeaseIdentity),
    GraphWarm(GraphWarmLeaseIdentity),
}
/// Wall-clock lease deadlines, in **absolute Unix epoch milliseconds** — never
/// durations.
///
/// The coordinator stores each field verbatim as a timestamp
/// (`BuildLeaseService::deadline` → `OffsetDateTime::from_unix_timestamp_nanos`)
/// and `expire_deadlines` compares those timestamps against its own wall clock.
/// A relative value therefore does not mean "in 30 seconds": `30_000` is
/// `1970-01-01T00:00:30Z`, permanently in the past, and every lease queued with
/// it is terminalized as `deadline_expired` the instant it is enqueued. Producers
/// must compute `now_ms + timeout_ms` (see `K8sGraphWarmer::spawn_warm_job` and
/// `LeaseInvocationRunner::lease_deadlines`).
///
/// A non-positive value means **no deadline**: `deadline()` maps it to `None`
/// and the durable column stays NULL, so the lease never expires on that edge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseDeadlines {
    /// Absolute epoch-millisecond instant after which a still-queued lease is
    /// terminalized as `deadline_expired`. Non-positive means no deadline.
    pub queue_deadline_ms: i64,
    /// Absolute epoch-millisecond instant after which a granted-but-unlaunched
    /// lease is marked `suspect`. Non-positive means no deadline.
    pub launch_deadline_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseFencingToken(pub u64);
/// Whether the current durable admission epoch authorizes the launcher to lift
/// the reserved cpu.max quota for a bound v1 (invocation) lease.
///
/// This is the agent/launcher-side projection of the admission handoff epoch.
/// It is deliberately small and fail-closed: only a committed overlap or
/// invocation-primary epoch with v1 enforcing yields [`Self::Lift`]. A shadow
/// epoch is observed but never lifts; every other epoch (baseline, missing,
/// unreadable, stale, or the illegal both-non-enforcing combo) keeps the quota
/// unleased.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationLiftDecision {
    /// The epoch is a committed overlap or invocation-primary phase with v1
    /// enforcing: a matching durable fencing token may lift cpu.max.
    Lift,
    /// v1 is shadowing: the invocation authority observes what it would do but
    /// never lifts. The launcher stays throttled under v0.
    Shadow,
    /// Baseline / missing / unreadable / stale / contradictory epoch: keep the
    /// launcher quota unleased. This is the fail-closed default.
    #[default]
    Unleased,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutCredit {
    pub units: u8,
    pub retry_after_ms: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseQueueRequest {
    pub identity: LeaseIdentity,
    pub deadlines: LeaseDeadlines,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrantRequest {
    pub identity: LeaseIdentity,
    pub fencing_token: LeaseFencingToken,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseStatusRequest {
    pub identity: LeaseIdentity,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseAbandonRequest {
    pub identity: LeaseIdentity,
    pub candidate_cleanup: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseBindRequest {
    pub identity: LeaseIdentity,
    pub fencing_token: LeaseFencingToken,
    pub pod_uid: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseCancelRequest {
    pub identity: LeaseIdentity,
    pub fencing_token: Option<LeaseFencingToken>,
    pub candidate_cleanup: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseReleaseRequest {
    pub identity: LeaseIdentity,
    pub fencing_token: LeaseFencingToken,
    pub candidate_cleanup: bool,
}
/// Fail-closed request to terminate exactly the immutable Pod recorded for a
/// task-run. Pod names are intentionally absent because they can be reused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogTerminationRequest {
    pub task_id: String,
    pub task_run_id: String,
    pub pod_uid: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseState {
    Queued,
    Granted,
    Launching,
    Bound,
    Active,
    /// Kubernetes reconciliation could not prove this counted lease safe.
    Suspect,
    Cancelled,
    Released,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseStatus {
    pub state: LeaseState,
    pub fencing_token: Option<LeaseFencingToken>,
    pub deadlines: LeaseDeadlines,
    pub pod_uid: Option<String>,
    pub candidate_cleanup: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub fencing_token: LeaseFencingToken,
    pub deadlines: LeaseDeadlines,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseResult {
    Queued(LeaseStatus),
    Granted(LeaseGrant),
    Status(LeaseStatus),
    Abandoned {
        candidate_cleanup: bool,
    },
    Bound(LeaseStatus),
    Cancelled {
        candidate_cleanup: bool,
    },
    Released {
        candidate_cleanup: bool,
    },
    LeaseIdentityConflict {
        identity: LeaseIdentity,
    },
    LeaseWaitTimeout {
        timeout_credit: Option<TimeoutCredit>,
    },
    LeaseUnavailable,
}
