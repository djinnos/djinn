//! Owned, serde-safe v1 lease protocol contracts.
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInvocationLeaseIdentity {
    pub task_id: String,
    pub task_run_id: String,
    pub invocation_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphWarmLeaseIdentity {
    pub project_id: String,
    pub warm_request_id: String,
    pub graph_revision: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseIdentity {
    TaskInvocation(TaskInvocationLeaseIdentity),
    GraphWarm(GraphWarmLeaseIdentity),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseDeadlines {
    pub queue_deadline_ms: i64,
    pub launch_deadline_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseFencingToken(pub u64);
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
