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
impl LeaseTelemetryOutcome {
    const fn as_label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Granted => "granted",
            Self::Timeout => "timeout",
            Self::Conflict => "conflict",
            Self::Unavailable => "unavailable",
            Self::Terminal => "terminal",
            Self::Status => "status",
        }
    }
}
impl LeaseTelemetryState {
    const fn as_label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Occupied => "occupied",
            Self::Terminal => "terminal",
            Self::NotReady => "not_ready",
        }
    }
}
impl LeaseOperation {
    const fn as_label(self) -> &'static str {
        match self {
            Self::Recover => "recover",
            Self::Queue => "queue",
            Self::Grant => "grant",
            Self::Status => "status",
            Self::Abandon => "abandon",
            Self::Bind => "bind",
            Self::Cancel => "cancel",
            Self::Release => "release",
            Self::Expire => "expire",
            Self::SetCap => "set_cap",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseTelemetryConsumer {
    TaskInvocation,
    GraphWarm,
    System,
}
impl LeaseTelemetryConsumer {
    const fn as_label(self) -> &'static str {
        match self {
            Self::TaskInvocation => "task_invocation",
            Self::GraphWarm => "graph_warm",
            Self::System => "system",
        }
    }
}
pub trait LeaseTelemetry: Send + Sync {
    fn state(&self, state: LeaseTelemetryState, count: u64);
    fn outcome(&self, outcome: LeaseTelemetryOutcome);
    fn operation(&self, operation: LeaseOperation, consumer: LeaseTelemetryConsumer);
}
#[derive(Default)]
pub struct NoopLeaseTelemetry;
impl LeaseTelemetry for NoopLeaseTelemetry {
    fn state(&self, _: LeaseTelemetryState, _: u64) {}
    fn outcome(&self, _: LeaseTelemetryOutcome) {}
    fn operation(&self, _: LeaseOperation, _: LeaseTelemetryConsumer) {}
}

#[derive(Default)]
pub struct MetricsLeaseTelemetry;
impl LeaseTelemetry for MetricsLeaseTelemetry {
    fn state(&self, state: LeaseTelemetryState, count: u64) {
        metrics::gauge!("djinn_build_lease_units", "state" => state.as_label()).set(count as f64);
    }
    fn outcome(&self, outcome: LeaseTelemetryOutcome) {
        metrics::counter!("djinn_build_lease_outcomes_total", "outcome" => outcome.as_label())
            .increment(1);
    }
    fn operation(&self, operation: LeaseOperation, consumer: LeaseTelemetryConsumer) {
        metrics::counter!("djinn_build_lease_operations_total", "operation" => operation.as_label(), "consumer" => consumer.as_label()).increment(1);
    }
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
            Arc::new(MetricsLeaseTelemetry),
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
        self.telemetry
            .operation(LeaseOperation::Recover, LeaseTelemetryConsumer::System);
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

    /// Return the durable non-terminal recovery view without mutating it.
    pub async fn recovery_snapshot(&self) -> Result<djinn_db::BuildLeaseSnapshot, ()> {
        if !self.is_ready() {
            return Err(());
        }
        self.repository.snapshot().await.map_err(|_| ())
    }

    /// Capacity changes never revoke occupied rows. Positive changes drain FIFO.
    pub async fn set_cap(&self, cap: i64) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() || cap < 0 {
            return self.unavailable();
        }
        self.telemetry
            .operation(LeaseOperation::SetCap, LeaseTelemetryConsumer::System);
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
        self.telemetry
            .operation(LeaseOperation::Queue, telemetry_consumer(&request.identity));
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
                if let Some(result) = self.queue_result(&row).await {
                    return result;
                }
                match self.drain().await {
                    // Re-read through idempotent queue after drain: it includes
                    // a newly minted grant or terminalized queue deadline.
                    Ok(_) => match self.repository.queue(&input).await {
                        Ok(QueueBuildLeaseResult::Queued { row, .. }) => {
                            self.queue_result(&row).await.unwrap_or_else(|| {
                                self.telemetry.outcome(LeaseTelemetryOutcome::Queued);
                                LeaseResult::Queued(status(&row))
                            })
                        }
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
        self.telemetry.operation(
            LeaseOperation::Status,
            telemetry_consumer(&request.identity),
        );
        self.pause.before_transaction(LeaseOperation::Status).await;
        let (key, _) = identity(&request.identity);
        match self.repository.get(&key).await {
            Ok(Some(row)) => {
                self.telemetry.outcome(LeaseTelemetryOutcome::Status);
                LeaseResult::Status(status(&row))
            }
            Ok(None) | Err(_) => self.unavailable(),
        }
    }

    pub async fn abandon(&self, request: LeaseAbandonRequest) -> LeaseResult {
        self.terminal(
            request.identity,
            None,
            request.candidate_cleanup,
            LeaseOperation::Abandon,
        )
        .await
    }
    pub async fn bind(&self, request: LeaseBindRequest) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.telemetry
            .operation(LeaseOperation::Bind, telemetry_consumer(&request.identity));
        self.pause.before_transaction(LeaseOperation::Bind).await;
        let (key, _) = identity(&request.identity);
        match self
            .repository
            .bind(&key, request.fencing_token.0 as i64, &request.pod_uid, None)
            .await
        {
            Ok(row) if row.state == BuildLeaseState::Bound => LeaseResult::Bound(status(&row)),
            Ok(row) => LeaseResult::Status(status(&row)),
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
    /// Persist an occupied reconciliation observation without releasing capacity.
    pub async fn report(
        &self,
        identity: LeaseIdentity,
        token: LeaseFencingToken,
        state: LeaseState,
    ) -> LeaseResult {
        let state = match state {
            LeaseState::Active => BuildLeaseState::Active,
            LeaseState::Suspect => BuildLeaseState::Suspect,
            _ => return self.unavailable(),
        };
        self.transition(identity, token, state, LeaseOperation::Status).await
    }
    pub async fn expire_deadlines(&self) -> LeaseResult {
        let _guard = self.operation.lock().await;
        if !self.is_ready() {
            return self.unavailable();
        }
        self.telemetry
            .operation(LeaseOperation::Expire, LeaseTelemetryConsumer::System);
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
        self.telemetry.operation(op, telemetry_consumer(&id));
        self.pause.before_transaction(op).await;
        let (key, _) = identity(&id);
        // A delayed grant acknowledgement must replay a forward durable state:
        // it may not change Bound back to Launching or lose its pod UID.
        match self.repository.get(&key).await {
            Ok(Some(row)) if row.fencing_token == Some(token.0 as i64) => match row.state {
                BuildLeaseState::Granted | BuildLeaseState::Launching => {}
                _ => return LeaseResult::Status(status(&row)),
            },
            Ok(Some(_)) | Ok(None) | Err(_) => return self.unavailable(),
        }
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
        self.telemetry.operation(op, telemetry_consumer(&id));
        self.pause.before_transaction(op).await;
        let (key, _) = identity(&id);
        let evidence = cleanup.then(|| serde_json::json!({"candidate_cleanup": true}));
        // Terminal rows are retained replay records. A supplied token remains
        // fenced even on retry, so stale callers cannot claim a terminal result.
        match self.repository.get(&key).await {
            Ok(Some(row)) if row.state == BuildLeaseState::Terminal => {
                if token.is_some_and(|value| row.fencing_token != Some(value.0 as i64)) {
                    return self.unavailable();
                }
                return terminal_result(&row);
            }
            // The contract has no abandon token; it may only remove a queued
            // row and can never revoke an occupied lease.
            Ok(Some(row))
                if op == LeaseOperation::Abandon && row.state != BuildLeaseState::Queued =>
            {
                return self.unavailable();
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return self.unavailable(),
        }
        let row = match op {
            LeaseOperation::Abandon => self.repository.abandon_queued(&key, evidence).await,
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
                terminal_result(&row)
            }
            Err(_) => self.unavailable(),
        }
    }

    async fn drain(&self) -> Result<Option<BuildLeaseRow>, ()> {
        let mut last = None;
        loop {
            self.telemetry
                .operation(LeaseOperation::Grant, LeaseTelemetryConsumer::System);
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
    async fn queue_result(&self, row: &BuildLeaseRow) -> Option<LeaseResult> {
        if row.state == BuildLeaseState::Terminal
            && row.terminal_reason.as_deref() == Some("deadline_expired")
        {
            let credit = self
                .repository
                .consume_timeout_credit(&row.key)
                .await
                .ok()?;
            return Some(LeaseResult::LeaseWaitTimeout {
                timeout_credit: credit.then_some(djinn_supervisor::services::TimeoutCredit {
                    units: 1,
                    retry_after_ms: 0,
                }),
            });
        }
        queue_result(row)
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
fn telemetry_consumer(id: &LeaseIdentity) -> LeaseTelemetryConsumer {
    match id {
        LeaseIdentity::TaskInvocation(_) => LeaseTelemetryConsumer::TaskInvocation,
        LeaseIdentity::GraphWarm(_) => LeaseTelemetryConsumer::GraphWarm,
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
        BuildLeaseState::Queued => None,
        BuildLeaseState::Granted => Some(granted_result(row)),
        BuildLeaseState::Launching
        | BuildLeaseState::Bound
        | BuildLeaseState::Active
        | BuildLeaseState::Suspect => Some(LeaseResult::Status(status(row))),
        BuildLeaseState::Terminal => Some(terminal_result(row)),
    }
}
fn terminal_result(row: &BuildLeaseRow) -> LeaseResult {
    match row.terminal_reason.as_deref() {
        Some("abandoned") => LeaseResult::Abandoned {
            candidate_cleanup: row.candidate_cleanup.is_some(),
        },
        Some("cancelled") => LeaseResult::Cancelled {
            candidate_cleanup: row.candidate_cleanup.is_some(),
        },
        Some("released") => LeaseResult::Released {
            candidate_cleanup: row.candidate_cleanup.is_some(),
        },
        Some("deadline_expired") => LeaseResult::LeaseWaitTimeout {
            timeout_credit: None,
        },
        _ => LeaseResult::LeaseUnavailable,
    }
}
fn status(row: &BuildLeaseRow) -> LeaseStatus {
    LeaseStatus {
        state: match row.state {
            BuildLeaseState::Queued => LeaseState::Queued,
            BuildLeaseState::Granted => LeaseState::Granted,
            BuildLeaseState::Launching => LeaseState::Launching,
            BuildLeaseState::Bound => LeaseState::Bound,
            BuildLeaseState::Active => LeaseState::Active,
            BuildLeaseState::Suspect => LeaseState::Suspect,
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn row(state: BuildLeaseState, terminal_reason: Option<&str>) -> BuildLeaseRow {
        BuildLeaseRow {
            key: BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::GraphWarm,
                consumer_id: "stable-request".into(),
            },
            immutable_identity: "warm:project:stable-request:revision".into(),
            enqueue_sequence: 1,
            fencing_token: (state != BuildLeaseState::Queued).then_some(17),
            state,
            queue_deadline: Some("2026-01-01T00:01:00Z".into()),
            launch_deadline: Some("2026-01-01T00:02:00Z".into()),
            bound_pod_uid: matches!(
                state,
                BuildLeaseState::Bound
                    | BuildLeaseState::Active
                    | BuildLeaseState::Suspect
                    | BuildLeaseState::Terminal
            )
            .then(|| "pod-a".into()),
            candidate_cleanup: None,
            terminal_reason: terminal_reason.map(str::to_owned),
            timeout_credit_consumed: false,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            granted_at: None,
            terminal_at: None,
        }
    }

    #[test]
    fn queue_replay_uses_typed_result_for_every_durable_state() {
        assert_eq!(queue_result(&row(BuildLeaseState::Queued, None)), None);
        assert!(matches!(
            queue_result(&row(BuildLeaseState::Granted, None)),
            Some(LeaseResult::Granted(_))
        ));
        for state in [
            BuildLeaseState::Launching,
            BuildLeaseState::Bound,
            BuildLeaseState::Active,
            BuildLeaseState::Suspect,
        ] {
            assert!(matches!(
                queue_result(&row(state, None)),
                Some(LeaseResult::Status(_))
            ));
        }
        assert!(matches!(
            queue_result(&row(BuildLeaseState::Terminal, Some("abandoned"))),
            Some(LeaseResult::Abandoned { .. })
        ));
        assert!(matches!(
            queue_result(&row(BuildLeaseState::Terminal, Some("cancelled"))),
            Some(LeaseResult::Cancelled { .. })
        ));
        assert!(matches!(
            queue_result(&row(BuildLeaseState::Terminal, Some("released"))),
            Some(LeaseResult::Released { .. })
        ));
        assert!(matches!(
            queue_result(&row(BuildLeaseState::Terminal, Some("deadline_expired"))),
            Some(LeaseResult::LeaseWaitTimeout { .. })
        ));
    }

    #[test]
    fn terminal_replay_uses_persisted_winner_and_cleanup() {
        let mut cancelled = row(BuildLeaseState::Terminal, Some("cancelled"));
        cancelled.candidate_cleanup = Some(serde_json::json!({"candidate_cleanup": true}));
        assert_eq!(
            terminal_result(&cancelled),
            LeaseResult::Cancelled {
                candidate_cleanup: true
            }
        );
        assert_eq!(
            terminal_result(&row(BuildLeaseState::Terminal, Some("released"))),
            LeaseResult::Released {
                candidate_cleanup: false
            }
        );
        assert_eq!(
            terminal_result(&row(BuildLeaseState::Terminal, Some("deadline_expired"))),
            LeaseResult::LeaseWaitTimeout {
                timeout_credit: None
            }
        );
    }

    #[test]
    fn production_metrics_use_only_bounded_typed_labels() {
        let (_, rendered) = djinn_telemetry::render_isolated(|| {
            let telemetry = MetricsLeaseTelemetry;
            for operation in [
                LeaseOperation::Queue,
                LeaseOperation::Grant,
                LeaseOperation::Status,
                LeaseOperation::Abandon,
                LeaseOperation::Bind,
                LeaseOperation::Cancel,
                LeaseOperation::Release,
            ] {
                telemetry.operation(operation, LeaseTelemetryConsumer::TaskInvocation);
                telemetry.operation(operation, LeaseTelemetryConsumer::GraphWarm);
            }
            for operation in [
                LeaseOperation::Recover,
                LeaseOperation::Grant,
                LeaseOperation::Expire,
                LeaseOperation::SetCap,
            ] {
                telemetry.operation(operation, LeaseTelemetryConsumer::System);
            }
            for state in [
                LeaseTelemetryState::Queued,
                LeaseTelemetryState::Occupied,
                LeaseTelemetryState::Terminal,
                LeaseTelemetryState::NotReady,
            ] {
                telemetry.state(state, 1);
            }
            for outcome in [
                LeaseTelemetryOutcome::Queued,
                LeaseTelemetryOutcome::Granted,
                LeaseTelemetryOutcome::Timeout,
                LeaseTelemetryOutcome::Conflict,
                LeaseTelemetryOutcome::Unavailable,
                LeaseTelemetryOutcome::Terminal,
                LeaseTelemetryOutcome::Status,
            ] {
                telemetry.outcome(outcome);
            }
        });

        let allowed_keys = BTreeSet::from(["operation", "consumer", "state", "outcome"]);
        let allowed_operations = BTreeSet::from([
            "recover", "queue", "grant", "status", "abandon", "bind", "cancel", "release",
            "expire", "set_cap",
        ]);
        let allowed_consumers = BTreeSet::from(["task_invocation", "graph_warm", "system"]);
        let allowed_states = BTreeSet::from(["queued", "occupied", "terminal", "not_ready"]);
        let allowed_outcomes = BTreeSet::from([
            "queued",
            "granted",
            "timeout",
            "conflict",
            "unavailable",
            "terminal",
            "status",
        ]);
        let mut seen_operations = BTreeSet::new();
        let mut seen_consumers = BTreeSet::new();
        let mut seen_states = BTreeSet::new();
        let mut seen_outcomes = BTreeSet::new();

        for line in rendered
            .lines()
            .filter(|line| line.starts_with("djinn_build_lease_") && !line.starts_with('#'))
        {
            let labels = line
                .split_once('{')
                .and_then(|(_, rest)| rest.split_once('}').map(|(labels, _)| labels))
                .unwrap_or("");
            let mut line_keys = BTreeSet::new();
            for label in labels.split(',').filter(|label| !label.is_empty()) {
                let (key, quoted_value) = label.split_once('=').unwrap_or((label, ""));
                let value = quoted_value.trim_matches('"');
                assert!(allowed_keys.contains(key), "unbounded metric key in {line}");
                line_keys.insert(key);
                if key == "operation" {
                    assert!(allowed_operations.contains(value));
                    seen_operations.insert(value);
                } else if key == "consumer" {
                    assert!(allowed_consumers.contains(value));
                    seen_consumers.insert(value);
                } else if key == "state" {
                    assert!(allowed_states.contains(value));
                    seen_states.insert(value);
                } else if key == "outcome" {
                    assert!(allowed_outcomes.contains(value));
                    seen_outcomes.insert(value);
                }
            }
            if line.starts_with("djinn_build_lease_operations_total") {
                assert_eq!(line_keys, BTreeSet::from(["operation", "consumer"]));
            } else if line.starts_with("djinn_build_lease_units") {
                assert_eq!(line_keys, BTreeSet::from(["state"]));
            } else if line.starts_with("djinn_build_lease_outcomes_total") {
                assert_eq!(line_keys, BTreeSet::from(["outcome"]));
            }
        }

        assert_eq!(seen_operations, allowed_operations);
        assert_eq!(seen_consumers, allowed_consumers);
        assert_eq!(seen_states, allowed_states);
        assert_eq!(seen_outcomes, allowed_outcomes);
        for identifier in [
            "task-run-secret-7f91",
            "pod-secret-7f91",
            "invocation-secret-7f91",
            "request-secret-7f91",
            "lease-secret-7f91",
            "fencing-secret-7f91",
        ] {
            assert!(!rendered.contains(identifier));
        }
        for forbidden_key in [
            "task_run_id",
            "pod_uid",
            "invocation_id",
            "request_id",
            "lease_id",
            "fencing_token",
        ] {
            assert!(!rendered.contains(&format!("{forbidden_key}=\"")));
        }
    }
}
