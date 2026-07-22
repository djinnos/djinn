//! Coordinator-owned v1 durable build-lease policy.
//!
//! The repository serializes durable mutations with its advisory lock; this
//! service adds readiness, deterministic clock/pause seams and contract mapping.
//! It is deliberately not connected to v0 dispatch or graph warming.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, Ordering},
};

use async_trait::async_trait;
use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildLeaseRow, BuildLeaseState,
    GrantNextBuildLeaseResult, QueueBuildLeaseInput, QueueBuildLeaseResult,
};
use djinn_supervisor::services::{
    LeaseAbandonRequest, LeaseBindRequest, LeaseCancelRequest, LeaseDeadlines, LeaseFencingToken,
    LeaseGrant, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest,
    LeaseResult, LeaseState, LeaseStatus, LeaseStatusRequest,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::Mutex;

/// Deterministic deadline clock seam, expressed in contract milliseconds.
pub trait LeaseClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

#[derive(Default)]
pub struct SystemLeaseClock;
impl LeaseClock for SystemLeaseClock {
    fn now_ms(&self) -> i64 {
        (OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
    }
}

/// Controllable clock used by the race suite without wall-clock sleeps.
pub struct ManualLeaseClock(AtomicI64);
impl ManualLeaseClock {
    #[must_use]
    pub const fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }
    pub fn set_ms(&self, now_ms: i64) {
        self.0.store(now_ms, Ordering::Release);
    }
}
impl LeaseClock for ManualLeaseClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Acquire)
    }
}

/// Transaction pause seam. The repository remains the cross-process ordering
/// authority; this hook lets tests arrange deterministic operation order.
#[async_trait]
pub trait LeaseTransactionPause: Send + Sync {
    async fn before_transaction(&self, operation: LeaseOperation);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseOperation {
    Recover,
    Queue,
    Grant,
    Status,
    Abandon,
    Bind,
    Cancel,
    Release,
    Expire,
    SetCap,
}

#[derive(Default)]
pub struct NoopLeaseTransactionPause;
#[async_trait]
impl LeaseTransactionPause for NoopLeaseTransactionPause {
    async fn before_transaction(&self, _: LeaseOperation) {}
}

/// Bounded telemetry values; callers may add identifiers only to tracing spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseTelemetryState {
    Queued,
    Occupied,
    Terminal,
    NotReady,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseTelemetryOutcome {
    Queued,
    Granted,
    Timeout,
    Conflict,
    Unavailable,
    Terminal,
    Status,
}
pub trait LeaseTelemetry: Send + Sync {
    fn state(&self, state: LeaseTelemetryState, count: u64);
    fn outcome(&self, outcome: LeaseTelemetryOutcome);
}
#[derive(Default)]
pub struct NoopLeaseTelemetry;
impl LeaseTelemetry for NoopLeaseTelemetry {
    fn state(&self, _: LeaseTelemetryState, _: u64) {}
    fn outcome(&self, _: LeaseTelemetryOutcome) {}
}

/// Coordinator policy owner over the one repository-global FIFO.
pub struct BuildLeaseService {
    repository: Arc<BuildLeaseRepository>,
    clock: Arc<dyn LeaseClock>,
    pause: Arc<dyn LeaseTransactionPause>,
    telemetry: Arc<dyn LeaseTelemetry>,
    cap: AtomicI64,
    recovered: AtomicBool,
    /// Keeps queue+local-drain atomic in this process. Database advisory locks
    /// serialize the same decisions across replacement coordinators.
    operation: Mutex<()>,
}

impl BuildLeaseService {
    #[must_use]
    pub fn new(repository: Arc<BuildLeaseRepository>, cap: i64) -> Self {
        Self::with_seams(
            repository,
            cap,
            Arc::new(SystemLeaseClock),
            Arc::new(NoopLeaseTransactionPause),
            Arc::new(NoopLeaseTelemetry),
        )
    }

    #[must_use]
    pub fn with_seams(
        repository: Arc<BuildLeaseRepository>,
        cap: i64,
        clock: Arc<dyn LeaseClock>,
        pause: Arc<dyn LeaseTransactionPause>,
        telemetry: Arc<dyn LeaseTelemetry>,
    ) -> Self {
        Self {
            repository,
            clock,
            pause,
            telemetry,
            cap: AtomicI64::new(cap.max(0)),
            recovered: AtomicBool::new(false),
            operation: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.recovered.load(Ordering::Acquire)
    }

    /// Recover queued and all occupied rows before opening the service.
    pub async fn recover(&self) -> LeaseResult {
        let _guard = self.operation.lock().await;
        self.pause.before_transaction(LeaseOperation::Recover).await;
        match self.repository.snapshot().await {
            Ok(snapshot) => {
                self.cap.store(snapshot.cap, Ordering::Release);
                self.publish(&snapshot.rows);
                self.recovered.store(true, Ordering::Release);
                LeaseResult::Status(empty_status())
            }
            Err(_) => self.unavailable(),
        }
    }

    /// Capacity changes never revoke occupied rows. Positive changes drain FIFO.
    pub async fn set_cap(&self, cap: i64) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() || cap < 0 {
            return self.unavailable();
        }
        self.pause.before_transaction(LeaseOperation::SetCap).await;
        match self.repository.set_cap(cap).await {
            Ok(_) => {
                self.cap.store(cap, Ordering::Release);
                let _ = self.drain().await;
                LeaseResult::Status(empty_status())
            }
            Err(_) => self.unavailable(),
        }
    }

    pub async fn queue(&self, request: LeaseQueueRequest) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.pause.before_transaction(LeaseOperation::Queue).await;
        let (key, immutable_identity) = identity(&request.identity);
        let input = QueueBuildLeaseInput {
            key: key.clone(),
            immutable_identity,
            queue_deadline: deadline(request.deadlines.queue_deadline_ms),
            launch_deadline: deadline(request.deadlines.launch_deadline_ms),
        };
        match self.repository.queue(&input).await {
            Ok(QueueBuildLeaseResult::LeaseIdentityConflict { .. }) => {
                self.telemetry.outcome(LeaseTelemetryOutcome::Conflict);
                LeaseResult::LeaseIdentityConflict {
                    identity: request.identity,
                }
            }
            Ok(QueueBuildLeaseResult::Queued { row, .. }) => {
                // Exact replay after a lost response must use the durable state
                // rather than whether this request happened to grant the row.
                if let Some(result) = queue_result(&row) {
                    return result;
                }
                match self.drain().await {
                    // Re-read through idempotent queue after drain: it includes
                    // a newly minted grant or terminalized queue deadline.
                    Ok(_) => match self.repository.queue(&input).await {
                        Ok(QueueBuildLeaseResult::Queued { row, .. }) => queue_result(&row)
                            .unwrap_or_else(|| {
                                self.telemetry.outcome(LeaseTelemetryOutcome::Queued);
                                LeaseResult::Queued(status(&row))
                            }),
                        Ok(QueueBuildLeaseResult::LeaseIdentityConflict { .. }) | Err(_) => {
                            self.unavailable()
                        }
                    },
                    Err(()) => self.unavailable(),
                }
            }
            Err(_) => self.unavailable(),
        }
    }

    /// A fenced grant acknowledgement moves the durable row to launching.
    pub async fn grant(&self, request: LeaseGrantRequest) -> LeaseResult {
        self.transition(
            request.identity,
            request.fencing_token,
            BuildLeaseState::Launching,
            LeaseOperation::Grant,
        )
        .await
    }

    pub async fn status(&self, request: LeaseStatusRequest) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.pause.before_transaction(LeaseOperation::Status).await;
        let (key, _) = identity(&request.identity);
        match self.repository.snapshot().await {
            Ok(snapshot) => snapshot
                .rows
                .into_iter()
                .find(|row| row.key == key)
                .map_or_else(
                    || self.unavailable(),
                    |row| {
                        self.telemetry.outcome(LeaseTelemetryOutcome::Status);
                        LeaseResult::Status(status(&row))
                    },
                ),
            Err(_) => self.unavailable(),
        }
    }

    pub async fn abandon(&self, request: LeaseAbandonRequest) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        // The contract has no fencing token: abandon can only terminalize a
        // queued request, and therefore can never revoke occupied work.
        self.pause.before_transaction(LeaseOperation::Abandon).await;
        let (key, _) = identity(&request.identity);
        match self.repository.snapshot().await {
            Ok(snapshot)
                if snapshot
                    .rows
                    .iter()
                    .any(|row| row.key == key && row.state == BuildLeaseState::Queued) =>
            {
                let evidence = request
                    .candidate_cleanup
                    .then(|| serde_json::json!({"candidate_cleanup": true}));
                match self.repository.cancel(&key, evidence).await {
                    Ok(row) => LeaseResult::Abandoned {
                        candidate_cleanup: row.candidate_cleanup.is_some(),
                    },
                    Err(_) => self.unavailable(),
                }
            }
            Ok(_) | Err(_) => self.unavailable(),
        }
    }
    pub async fn bind(&self, request: LeaseBindRequest) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.pause.before_transaction(LeaseOperation::Bind).await;
        let (key, _) = identity(&request.identity);
        match self
            .repository
            .bind(&key, request.fencing_token.0 as i64, &request.pod_uid, None)
            .await
        {
            Ok(row) => LeaseResult::Bound(status(&row)),
            Err(_) => self.unavailable(),
        }
    }
    pub async fn cancel(&self, request: LeaseCancelRequest) -> LeaseResult {
        self.terminal(
            request.identity,
            request.fencing_token,
            request.candidate_cleanup,
            LeaseOperation::Cancel,
        )
        .await
    }
    pub async fn release(&self, request: LeaseReleaseRequest) -> LeaseResult {
        self.terminal(
            request.identity,
            Some(request.fencing_token),
            request.candidate_cleanup,
            LeaseOperation::Release,
        )
        .await
    }
    pub async fn expire_deadlines(&self) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.pause.before_transaction(LeaseOperation::Expire).await;
        match self
            .repository
            .expire_deadlines(&timestamp(self.clock.now_ms()))
            .await
        {
            Ok(rows) => {
                if rows
                    .iter()
                    .any(|row| row.terminal_reason.as_deref() == Some("deadline_expired"))
                {
                    self.telemetry.outcome(LeaseTelemetryOutcome::Timeout);
                }
                let _ = self.drain().await;
                LeaseResult::Status(empty_status())
            }
            Err(_) => self.unavailable(),
        }
    }

    async fn transition(
        &self,
        id: LeaseIdentity,
        token: LeaseFencingToken,
        state: BuildLeaseState,
        op: LeaseOperation,
    ) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.pause.before_transaction(op).await;
        let (key, _) = identity(&id);
        match self
            .repository
            .status(&key, token.0 as i64, state, None)
            .await
        {
            Ok(row) => LeaseResult::Status(status(&row)),
            Err(_) => self.unavailable(),
        }
    }

    async fn terminal(
        &self,
        id: LeaseIdentity,
        token: Option<LeaseFencingToken>,
        cleanup: bool,
        op: LeaseOperation,
    ) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.pause.before_transaction(op).await;
        let (key, _) = identity(&id);
        let evidence = cleanup.then(|| serde_json::json!({"candidate_cleanup": true}));
        let row = match op {
            LeaseOperation::Abandon => self.repository.cancel(&key, evidence).await,
            LeaseOperation::Cancel => match token {
                Some(token) => {
                    self.repository
                        .cancel_fenced(&key, token.0 as i64, evidence)
                        .await
                }
                None => self.repository.cancel(&key, evidence).await,
            },
            LeaseOperation::Release => match token {
                Some(token) => {
                    self.repository
                        .release(&key, token.0 as i64, evidence)
                        .await
                }
                None => return self.unavailable(),
            },
            _ => return self.unavailable(),
        };
        match row {
            Ok(row) => {
                let _ = self.drain().await;
                self.telemetry.outcome(LeaseTelemetryOutcome::Terminal);
                match op {
                    LeaseOperation::Abandon => LeaseResult::Abandoned {
                        candidate_cleanup: row.candidate_cleanup.is_some(),
                    },
                    LeaseOperation::Cancel => LeaseResult::Cancelled {
                        candidate_cleanup: row.candidate_cleanup.is_some(),
                    },
                    _ => LeaseResult::Released {
                        candidate_cleanup: row.candidate_cleanup.is_some(),
                    },
                }
            }
            Err(_) => self.unavailable(),
        }
    }

    async fn drain(&self) -> Result<Option<BuildLeaseRow>, ()> {
        let mut last = None;
        loop {
            self.pause.before_transaction(LeaseOperation::Grant).await;
            match self
                .repository
                .grant_next(
                    self.cap.load(Ordering::Acquire),
                    &timestamp(self.clock.now_ms()),
                    None,
                )
                .await
                .map_err(|_| ())?
            {
                GrantNextBuildLeaseResult::Granted(row) => {
                    self.telemetry.outcome(LeaseTelemetryOutcome::Granted);
                    last = Some(row);
                }
                GrantNextBuildLeaseResult::Empty { .. } => return Ok(last),
            }
        }
    }
    fn unavailable(&self) -> LeaseResult {
        self.telemetry.outcome(LeaseTelemetryOutcome::Unavailable);
        LeaseResult::LeaseUnavailable
    }
    fn publish(&self, rows: &[BuildLeaseRow]) {
        let (queued, occupied) = rows
            .iter()
            .fold((0_u64, 0_u64), |(q, o), row| match row.state {
                BuildLeaseState::Queued => (q + 1, o),
                BuildLeaseState::Terminal => (q, o),
                _ => (q, o + 1),
            });
        self.telemetry.state(LeaseTelemetryState::Queued, queued);
        self.telemetry
            .state(LeaseTelemetryState::Occupied, occupied);
    }
}

fn identity(id: &LeaseIdentity) -> (BuildLeaseKey, String) {
    match id {
        LeaseIdentity::TaskInvocation(v) => (
            BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
                consumer_id: v.invocation_id.clone(),
            },
            format!("task:{}:{}:{}", v.task_id, v.task_run_id, v.invocation_id),
        ),
        LeaseIdentity::GraphWarm(v) => (
            BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::GraphWarm,
                consumer_id: v.warm_request_id.clone(),
            },
            format!(
                "warm:{}:{}:{}",
                v.project_id, v.warm_request_id, v.graph_revision
            ),
        ),
    }
}
fn timestamp(ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_default()
}
fn deadline(ms: i64) -> Option<String> {
    (ms > 0).then(|| timestamp(ms))
}
fn ms(value: Option<&str>) -> i64 {
    value
        .and_then(|v| OffsetDateTime::parse(v, &Rfc3339).ok())
        .map_or(0, |v| (v.unix_timestamp_nanos() / 1_000_000) as i64)
}
fn deadlines(row: &BuildLeaseRow) -> LeaseDeadlines {
    LeaseDeadlines {
        queue_deadline_ms: ms(row.queue_deadline.as_deref()),
        launch_deadline_ms: ms(row.launch_deadline.as_deref()),
    }
}
fn empty_status() -> LeaseStatus {
    LeaseStatus {
        state: LeaseState::Queued,
        fencing_token: None,
        deadlines: LeaseDeadlines {
            queue_deadline_ms: 0,
            launch_deadline_ms: 0,
        },
        pod_uid: None,
        candidate_cleanup: false,
    }
}
fn granted_result(row: &BuildLeaseRow) -> LeaseResult {
    LeaseResult::Granted(LeaseGrant {
        fencing_token: LeaseFencingToken(row.fencing_token.unwrap_or_default() as u64),
        deadlines: deadlines(row),
    })
}
/// Queue responses are reconstructed from durable state for idempotent grant
/// and queue-timeout retries. `None` means the row is still awaiting capacity.
fn queue_result(row: &BuildLeaseRow) -> Option<LeaseResult> {
    match row.state {
        BuildLeaseState::Granted => Some(granted_result(row)),
        BuildLeaseState::Terminal if row.terminal_reason.as_deref() == Some("deadline_expired") => {
            Some(LeaseResult::LeaseWaitTimeout {
                timeout_credit: None,
            })
        }
        _ => None,
    }
}
fn status(row: &BuildLeaseRow) -> LeaseStatus {
    LeaseStatus {
        state: match row.state {
            BuildLeaseState::Queued => LeaseState::Queued,
            BuildLeaseState::Granted => LeaseState::Granted,
            BuildLeaseState::Launching => LeaseState::Launching,
            BuildLeaseState::Bound => LeaseState::Bound,
            BuildLeaseState::Active | BuildLeaseState::Suspect => LeaseState::Active,
            BuildLeaseState::Terminal => {
                if row.terminal_reason.as_deref() == Some("released") {
                    LeaseState::Released
                } else {
                    LeaseState::Cancelled
                }
            }
        },
        fencing_token: row.fencing_token.map(|v| LeaseFencingToken(v as u64)),
        deadlines: deadlines(row),
        pod_uid: row.bound_pod_uid.clone(),
        candidate_cleanup: row.candidate_cleanup.is_some(),
    }
}
