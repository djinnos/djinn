//! Durable v1 build-lease ledger.
//!
//! Every mutation takes the same transaction-scoped advisory lock.  This makes
//! FIFO selection and occupancy accounting a single serializable decision while
//! retaining terminal rows for idempotent replay and audit.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::{Database, Error as DbError, Result as DbResult};

const OCCUPYING: [&str; 5] = ["granted", "launching", "bound", "active", "suspect"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildLeaseConsumerKind {
    TaskInvocation,
    GraphWarm,
}
impl BuildLeaseConsumerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::TaskInvocation => "task_invocation",
            Self::GraphWarm => "graph_warm",
        }
    }
    fn parse(s: &str) -> DbResult<Self> {
        match s {
            "task_invocation" => Ok(Self::TaskInvocation),
            "graph_warm" => Ok(Self::GraphWarm),
            _ => Err(DbError::InvalidData(format!(
                "invalid build lease consumer kind `{s}`"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildLeaseState {
    Queued,
    Granted,
    Launching,
    Bound,
    Active,
    Suspect,
    Terminal,
}
impl BuildLeaseState {
    fn parse(s: &str) -> DbResult<Self> {
        match s {
            "queued" => Ok(Self::Queued),
            "granted" => Ok(Self::Granted),
            "launching" => Ok(Self::Launching),
            "bound" => Ok(Self::Bound),
            "active" => Ok(Self::Active),
            "suspect" => Ok(Self::Suspect),
            "terminal" => Ok(Self::Terminal),
            _ => Err(DbError::InvalidData(format!(
                "invalid build lease state `{s}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLeaseKey {
    pub consumer_kind: BuildLeaseConsumerKind,
    pub consumer_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueBuildLeaseInput {
    pub key: BuildLeaseKey,
    /// Canonical, stable representation of all immutable invocation/warm fields.
    pub immutable_identity: String,
    /// RFC3339 timestamp, interpreted by PostgreSQL.
    pub queue_deadline: Option<String>,
    /// Absolute deadline retained while queued and carried into its grant.
    pub launch_deadline: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLeaseRow {
    pub key: BuildLeaseKey,
    pub immutable_identity: String,
    pub enqueue_sequence: i64,
    pub fencing_token: Option<i64>,
    pub state: BuildLeaseState,
    /// RFC3339 UTC with millisecond precision, rendered by the shared column
    /// list. `None` means the row carries no deadline at all.
    pub queue_deadline: Option<String>,
    /// RFC3339 UTC with millisecond precision, rendered by the shared column
    /// list. `None` means the row carries no deadline at all.
    pub launch_deadline: Option<String>,
    pub bound_pod_uid: Option<String>,
    pub candidate_cleanup: Option<serde_json::Value>,
    pub terminal_reason: Option<String>,
    pub timeout_credit_consumed: bool,
    pub created_at: String,
    pub updated_at: String,
    pub granted_at: Option<String>,
    pub terminal_at: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueBuildLeaseResult {
    Queued {
        row: BuildLeaseRow,
        idempotent: bool,
    },
    LeaseIdentityConflict {
        existing: BuildLeaseRow,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum GrantNextBuildLeaseResult {
    Granted(BuildLeaseRow),
    Empty { occupancy: i64, cap: i64 },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLeaseSnapshot {
    pub cap: i64,
    pub occupied: i64,
    pub rows: Vec<BuildLeaseRow>,
}

/// The full observed identity of one occupying lease, gathered *before* its
/// Kubernetes object was proven absent.
///
/// Every field participates in the compare-and-set performed by
/// [`BuildLeaseRepository::reclaim_absent_object`]. `observed_updated_at` is
/// what closes the race against the lease's own holder: any acknowledgement,
/// bind, or status report between the absence proof and the write bumps
/// `updated_at`, and the reclamation is fenced instead of retiring live work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimAbsentBuildLeaseInput {
    pub key: BuildLeaseKey,
    pub observed_state: BuildLeaseState,
    pub observed_immutable_identity: String,
    pub observed_fencing_token: Option<i64>,
    pub observed_bound_pod_uid: Option<String>,
    pub observed_updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReclaimAbsentBuildLeaseOutcome {
    Reclaimed(BuildLeaseRow),
    AlreadyTerminal(BuildLeaseRow),
    Fenced { reason: String },
}

/// Atomic repository for queueing and fencing build units.
pub struct BuildLeaseRepository {
    db: Database,
}
impl BuildLeaseRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Queue a stable identity. Exact replay returns the original row; any
    /// immutable mismatch is a typed result rather than a capacity side effect.
    pub async fn queue(&self, input: &QueueBuildLeaseInput) -> DbResult<QueueBuildLeaseResult> {
        if input.key.consumer_id.trim().is_empty() || input.immutable_identity.trim().is_empty() {
            return Err(DbError::InvalidData(
                "build lease identity must not be blank".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        if let Some(row) = fetch(&mut tx, &input.key, false).await? {
            tx.commit().await?;
            return Ok(if row.immutable_identity == input.immutable_identity {
                QueueBuildLeaseResult::Queued {
                    row,
                    idempotent: true,
                }
            } else {
                QueueBuildLeaseResult::LeaseIdentityConflict { existing: row }
            });
        }
        let row = sqlx::query_as::<_, DbRow>(&format!("INSERT INTO build_leases (consumer_kind,consumer_id,immutable_identity,queue_deadline,launch_deadline,state) VALUES ($1,$2,$3,$4::timestamptz,$5::timestamptz,'queued') RETURNING {COLS}"))
            .bind(input.key.consumer_kind.as_str()).bind(&input.key.consumer_id).bind(&input.immutable_identity).bind(&input.queue_deadline).bind(&input.launch_deadline).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(QueueBuildLeaseResult::Queued {
            row: row.try_into()?,
            idempotent: false,
        })
    }

    /// Reconcile the durable cap, then select only the oldest non-expired queue
    /// entry if all already granted/launching/bound/active/suspect units fit.
    pub async fn grant_next(
        &self,
        cap: i64,
        now: &str,
        launch_deadline: Option<&str>,
    ) -> DbResult<GrantNextBuildLeaseResult> {
        validate_cap(cap)?;
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        set_cap_tx(&mut tx, cap).await?;
        expire_queued_tx(&mut tx, now).await?;
        let occupied = occupancy_tx(&mut tx).await?;
        if occupied >= cap {
            tx.commit().await?;
            return Ok(GrantNextBuildLeaseResult::Empty {
                occupancy: occupied,
                cap,
            });
        }
        let candidate: Option<(String, String)> = sqlx::query_as("SELECT consumer_kind, consumer_id FROM build_leases WHERE state='queued' AND (queue_deadline IS NULL OR queue_deadline > $1::timestamptz) ORDER BY enqueue_sequence FOR UPDATE SKIP LOCKED LIMIT 1").bind(now).fetch_optional(&mut *tx).await?;
        let Some((kind, id)) = candidate else {
            tx.commit().await?;
            return Ok(GrantNextBuildLeaseResult::Empty {
                occupancy: occupied,
                cap,
            });
        };
        // Coordinator-originated deadlines are durable queue data. Direct
        // repository users may still supply one at grant time, but may not
        // erase a deadline already retained on the queued row.
        let row = sqlx::query_as::<_, DbRow>(&format!("UPDATE build_leases SET state='granted', fencing_token=nextval('build_lease_fencing_token_seq'), granted_at=$1::timestamptz, launch_deadline=COALESCE($2::timestamptz, launch_deadline), updated_at=now() WHERE consumer_kind=$3 AND consumer_id=$4 RETURNING {COLS}"))
            .bind(now).bind(launch_deadline).bind(kind).bind(id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(GrantNextBuildLeaseResult::Granted(row.try_into()?))
    }

    pub async fn set_cap(&self, cap: i64) -> DbResult<BuildLeaseSnapshot> {
        validate_cap(cap)?;
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        set_cap_tx(&mut tx, cap).await?;
        let snapshot = snapshot_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(snapshot)
    }
    pub async fn snapshot(&self) -> DbResult<BuildLeaseSnapshot> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let snapshot = snapshot_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    /// Look up one durable row, including a retained terminal outcome. Service
    /// retries use this instead of `snapshot`, whose recovery view excludes
    /// terminal rows.
    pub async fn get(&self, key: &BuildLeaseKey) -> DbResult<Option<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, false).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Every task-invocation lease that has been fenced onto a concrete pod.
    ///
    /// `bound_pod_uid` is the ONLY durable pod-UID evidence this database holds:
    /// the schema's `build_leases_bound_uid_check` constraint permits it solely
    /// in the pod-bound and terminal states, and it is written exclusively by
    /// the fencing-token `bind` path. Run-dir reconciliation (proposal nquz)
    /// therefore reads it as authoritative ownership; every directory without a
    /// matching row stays quarantined. Read-only and lock-free: it is a startup
    /// inventory query, never part of a lease decision.
    pub async fn list_pod_bound_task_invocations(&self) -> DbResult<Vec<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, DbRow>(&format!(
            "SELECT {COLS} FROM build_leases \
             WHERE consumer_kind='task_invocation' AND bound_pod_uid IS NOT NULL \
             ORDER BY consumer_id"
        ))
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter().map(BuildLeaseRow::try_from).collect()
    }

    /// Atomically consume the one retry credit attached to a committed queue
    /// deadline. False covers both replay and non-timeout terminal rows.
    pub async fn consume_timeout_credit(&self, key: &BuildLeaseKey) -> DbResult<bool> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let consumed = sqlx::query_scalar::<_, bool>(
            "UPDATE build_leases SET timeout_credit_consumed=TRUE, updated_at=now() \
             WHERE consumer_kind=$1 AND consumer_id=$2 AND state='terminal' \
             AND terminal_reason='deadline_expired' AND timeout_credit_consumed=FALSE \
             RETURNING TRUE",
        )
        .bind(key.consumer_kind.as_str())
        .bind(&key.consumer_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        tx.commit().await?;
        Ok(consumed)
    }

    /// Status reports are idempotent, token-fenced and cannot resurrect terminal work.
    pub async fn status(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        state: BuildLeaseState,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        // Granting is allocator-owned: it mints the fencing token. A status
        // report therefore starts at Launching and cannot move work backwards.
        if matches!(
            state,
            BuildLeaseState::Queued | BuildLeaseState::Granted | BuildLeaseState::Terminal
        ) {
            return Err(DbError::InvalidData(
                "status must be a post-grant occupied lifecycle state".into(),
            ));
        }
        // Status reports are acknowledgements, never commands to move a lease
        // backwards. In particular, a delayed Launching acknowledgement after
        // bind must replay the bound row rather than overwrite its pod binding.
        let allowed = match state {
            BuildLeaseState::Launching => {
                &[BuildLeaseState::Granted, BuildLeaseState::Launching][..]
            }
            BuildLeaseState::Bound => &[
                BuildLeaseState::Granted,
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
            ][..],
            BuildLeaseState::Active => &[
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
                BuildLeaseState::Active,
                // A complete retry may clear an uncertain Kubernetes
                // observation after the immutable binding is re-confirmed.
                BuildLeaseState::Suspect,
            ][..],
            BuildLeaseState::Suspect => &[
                BuildLeaseState::Granted,
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
                BuildLeaseState::Active,
                BuildLeaseState::Suspect,
            ][..],
            BuildLeaseState::Queued | BuildLeaseState::Granted | BuildLeaseState::Terminal => {
                unreachable!("validated above")
            }
        };
        self.transition(key, token, allowed, state, None, cleanup)
            .await
    }
    /// Atomically abandon only an ungranted queued request. Terminal rows are
    /// replayed so a lost abandon response remains idempotent.
    pub async fn abandon_queued(
        &self,
        key: &BuildLeaseKey,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if row.state == BuildLeaseState::Terminal {
            tx.commit().await?;
            return Ok(row);
        }
        if row.state != BuildLeaseState::Queued {
            return Err(DbError::InvalidTransition(
                "cannot abandon occupied build lease".into(),
            ));
        }
        let result = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_leases SET state='terminal',terminal_reason='abandoned',candidate_cleanup=COALESCE($1, candidate_cleanup),terminal_at=now(),updated_at=now() WHERE consumer_kind=$2 AND consumer_id=$3 RETURNING {COLS}"
        ))
        .bind(cleanup.map(sqlx::types::Json))
        .bind(key.consumer_kind.as_str())
        .bind(&key.consumer_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        result.try_into()
    }
    pub async fn cancel(
        &self,
        key: &BuildLeaseKey,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(key, None, "cancelled", cleanup).await
    }
    /// Fenced counterpart for a cancel request that carries a grant token.
    pub async fn cancel_fenced(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(key, Some(token), "cancelled", cleanup).await
    }
    pub async fn release(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(key, Some(token), "released", cleanup).await
    }

    /// Bind is the sole operation which can set a pod UID; the database trigger
    /// additionally makes that UID permanently immutable.
    pub async fn bind(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        pod_uid: &str,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        if pod_uid.trim().is_empty() {
            return Err(DbError::InvalidData("pod UID must not be blank".into()));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if row.fencing_token != Some(token) {
            return Err(DbError::InvalidTransition(
                "stale build lease fencing token".into(),
            ));
        }
        if row
            .bound_pod_uid
            .as_deref()
            .is_some_and(|uid| uid != pod_uid)
        {
            return Err(DbError::InvalidTransition(
                "pod UID does not match build lease".into(),
            ));
        }
        // A delayed bind with the committed immutable pod identity replays the
        // forward row without moving Active, Suspect, or Terminal backward.
        if matches!(
            row.state,
            BuildLeaseState::Bound
                | BuildLeaseState::Active
                | BuildLeaseState::Suspect
                | BuildLeaseState::Terminal
        ) && row.bound_pod_uid.as_deref() == Some(pod_uid)
        {
            tx.commit().await?;
            return Ok(row);
        }
        if !matches!(
            row.state,
            BuildLeaseState::Granted | BuildLeaseState::Launching
        ) {
            return Err(DbError::InvalidTransition(format!(
                "cannot bind build lease from {:?}",
                row.state
            )));
        }
        let result = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_leases SET state='bound',bound_pod_uid=$1,candidate_cleanup=COALESCE($2,candidate_cleanup),updated_at=now() WHERE consumer_kind=$3 AND consumer_id=$4 RETURNING {COLS}"
        ))
        .bind(pod_uid)
        .bind(cleanup.map(sqlx::types::Json))
        .bind(key.consumer_kind.as_str())
        .bind(&key.consumer_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        result.try_into()
    }
    pub async fn expire_deadlines(&self, now: &str) -> DbResult<Vec<BuildLeaseRow>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let rows=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state='terminal', terminal_reason='deadline_expired', terminal_at=$1::timestamptz, updated_at=now() WHERE state='queued' AND queue_deadline <= $1::timestamptz RETURNING {COLS}")).bind(now).fetch_all(&mut *tx).await?;
        let launch=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state='suspect', updated_at=now() WHERE state IN ('granted','launching') AND launch_deadline <= $1::timestamptz RETURNING {COLS}")).bind(now).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        rows.into_iter()
            .chain(launch)
            .map(TryInto::try_into)
            .collect()
    }

    /// Every currently occupying lease, each paired with whether it has been
    /// untouched for at least `settle_seconds`.
    ///
    /// Read-only and lock-free: reclamation gathers evidence from this listing
    /// and then fences its write on the row's `updated_at`, so a lease that
    /// moves between the listing and the write is rejected rather than raced.
    pub async fn list_occupying_with_settlement(
        &self,
        settle_seconds: i64,
    ) -> DbResult<Vec<(BuildLeaseRow, bool)>> {
        if settle_seconds < 0 {
            return Err(DbError::InvalidData(
                "settle window must be non-negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, SettlementDbRow>(&format!(
            "SELECT {COLS}, (updated_at <= now() - make_interval(secs => $2)) AS settled \
             FROM build_leases WHERE state = ANY($1) ORDER BY enqueue_sequence"
        ))
        .bind(OCCUPYING.as_slice())
        .bind(settle_seconds as f64)
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                let settled = row.settled.unwrap_or(false);
                BuildLeaseRow::try_from(row.row).map(|row| (row, settled))
            })
            .collect()
    }

    /// Retire one occupying lease whose Kubernetes object is provably gone.
    ///
    /// Nothing else in this ledger can leave an occupying state without a call
    /// from the process that holds the lease: `release`/`cancel` need the
    /// fencing token, `abandon_queued` refuses anything past `queued`, and
    /// `expire_deadlines` only downgrades `granted`/`launching` to the equally
    /// occupying `suspect`. A lease whose holder died — or, the production
    /// shape this fixes, a `granted` row whose requester gave up before it was
    /// ever acknowledged — therefore occupies the shared cap forever and
    /// starves every later build. This is the durable half of the reconciler
    /// that retires it; the caller owns the Kubernetes absence evidence.
    ///
    /// The write is a compare-and-set on the full observed identity (state,
    /// immutable identity, fencing token, bound pod UID, and `updated_at`).
    /// Anything that changed after the proof was gathered yields
    /// [`ReclaimAbsentBuildLeaseOutcome::Fenced`] and writes nothing, so this
    /// can never retire a lease that might still correspond to live work.
    pub async fn reclaim_absent_object(
        &self,
        input: &ReclaimAbsentBuildLeaseInput,
    ) -> DbResult<ReclaimAbsentBuildLeaseOutcome> {
        if !OCCUPYING.contains(&state_str(input.observed_state)) {
            return Err(DbError::InvalidData(
                "reclamation evidence must describe an occupying build lease state".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let Some(row) = fetch(&mut tx, &input.key, true).await? else {
            tx.commit().await?;
            return Ok(ReclaimAbsentBuildLeaseOutcome::Fenced {
                reason: "build lease no longer exists".into(),
            });
        };
        if row.state == BuildLeaseState::Terminal {
            tx.commit().await?;
            return Ok(ReclaimAbsentBuildLeaseOutcome::AlreadyTerminal(row));
        }
        if row.state != input.observed_state
            || row.immutable_identity != input.observed_immutable_identity
            || row.fencing_token != input.observed_fencing_token
            || row.bound_pod_uid != input.observed_bound_pod_uid
            || row.updated_at != input.observed_updated_at
        {
            let reason = format!(
                "build lease changed after the absence proof (observed {:?}@{}, found {:?}@{})",
                input.observed_state, input.observed_updated_at, row.state, row.updated_at
            );
            tx.commit().await?;
            return Ok(ReclaimAbsentBuildLeaseOutcome::Fenced { reason });
        }
        let reclaimed = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_leases SET state='terminal',terminal_reason='reclaimed_absent',\
             terminal_at=now(),updated_at=now() WHERE consumer_kind=$1 AND consumer_id=$2 \
             RETURNING {COLS}"
        ))
        .bind(input.key.consumer_kind.as_str())
        .bind(&input.key.consumer_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ReclaimAbsentBuildLeaseOutcome::Reclaimed(
            reclaimed.try_into()?,
        ))
    }

    async fn terminal(
        &self,
        key: &BuildLeaseKey,
        token: Option<i64>,
        reason: &str,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if token.is_some() && row.fencing_token != token {
            return Err(DbError::InvalidTransition(
                "stale build lease fencing token".into(),
            ));
        }
        if row.state == BuildLeaseState::Terminal {
            tx.commit().await?;
            return Ok(row);
        }
        let result=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state='terminal',terminal_reason=$1,candidate_cleanup=COALESCE($2, candidate_cleanup),terminal_at=now(),updated_at=now() WHERE consumer_kind=$3 AND consumer_id=$4 RETURNING {COLS}")).bind(reason).bind(cleanup.map(sqlx::types::Json)).bind(key.consumer_kind.as_str()).bind(&key.consumer_id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        result.try_into()
    }
    async fn transition(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        allowed: &[BuildLeaseState],
        state: BuildLeaseState,
        pod_uid: Option<&str>,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock(&mut tx).await?;
        let row = fetch(&mut tx, key, true)
            .await?
            .ok_or_else(|| DbError::InvalidTransition("unknown build lease".into()))?;
        if row.fencing_token != Some(token) {
            return Err(DbError::InvalidTransition(
                "stale build lease fencing token".into(),
            ));
        }
        // Close the race between a service replay read and this locked
        // transition. A delayed grant acknowledgement requests Launching, but
        // any later durable state is already its successful forward outcome.
        if state == BuildLeaseState::Launching
            && matches!(
                row.state,
                BuildLeaseState::Bound
                    | BuildLeaseState::Active
                    | BuildLeaseState::Suspect
                    | BuildLeaseState::Terminal
            )
        {
            tx.commit().await?;
            return Ok(row);
        }
        if row.state == state
            && cleanup.is_none()
            && (pod_uid.is_none() || row.bound_pod_uid.as_deref() == pod_uid)
        {
            tx.commit().await?;
            return Ok(row);
        }
        if !allowed.contains(&row.state) {
            return Err(DbError::InvalidTransition(format!(
                "cannot transition build lease from {:?}",
                row.state
            )));
        }
        if row.bound_pod_uid.is_some()
            && pod_uid.is_some()
            && row.bound_pod_uid.as_deref() != pod_uid
        {
            return Err(DbError::InvalidTransition(
                "pod UID does not match build lease".into(),
            ));
        }
        let state = match state {
            BuildLeaseState::Launching => "launching",
            BuildLeaseState::Bound => "bound",
            BuildLeaseState::Active => "active",
            BuildLeaseState::Suspect => "suspect",
            _ => unreachable!(),
        };
        let result=sqlx::query_as::<_,DbRow>(&format!("UPDATE build_leases SET state=$1,bound_pod_uid=COALESCE($2,bound_pod_uid),candidate_cleanup=COALESCE($3,candidate_cleanup),updated_at=now() WHERE consumer_kind=$4 AND consumer_id=$5 RETURNING {COLS}")).bind(state).bind(pod_uid).bind(cleanup.map(sqlx::types::Json)).bind(key.consumer_kind.as_str()).bind(&key.consumer_id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        result.try_into()
    }
}

/// The single projection every read and `RETURNING` clause of this repository
/// uses.
///
/// Every `timestamptz` is rendered as RFC3339 in UTC with millisecond
/// precision, which is the *only* timestamp representation this repository
/// emits or accepts: callers bind RFC3339 in (`$n::timestamptz`) and read
/// RFC3339 back out, so one parser round-trips the values.
///
/// A plain `::text` cast would instead yield PostgreSQL's own output format
/// (`2026-07-25 20:30:00+00`, space-separated, no `T`). That is not RFC3339,
/// and the coordinator's `build_lease::ms` — the sole reader of the echoed
/// deadlines — parses RFC3339 and maps a parse failure to `0`. Since `0` is
/// itself meaningful in that contract ("no deadline"), the cast silently turned
/// every real deadline into "unbounded" on the way out. Keep the rendering
/// here; do not add a second parser on the Rust side.
const COLS: &str = "consumer_kind,consumer_id,immutable_identity,enqueue_sequence,fencing_token,state,\
    to_char(queue_deadline AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS queue_deadline,\
    to_char(launch_deadline AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS launch_deadline,\
    bound_pod_uid,candidate_cleanup,terminal_reason,timeout_credit_consumed,\
    to_char(created_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at,\
    to_char(updated_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS updated_at,\
    to_char(granted_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS granted_at,\
    to_char(terminal_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS terminal_at";
#[derive(sqlx::FromRow)]
struct SettlementDbRow {
    #[sqlx(flatten)]
    row: DbRow,
    settled: Option<bool>,
}
#[derive(sqlx::FromRow)]
struct DbRow {
    consumer_kind: String,
    consumer_id: String,
    immutable_identity: String,
    enqueue_sequence: i64,
    fencing_token: Option<i64>,
    state: String,
    queue_deadline: Option<String>,
    launch_deadline: Option<String>,
    bound_pod_uid: Option<String>,
    candidate_cleanup: Option<sqlx::types::Json<serde_json::Value>>,
    terminal_reason: Option<String>,
    timeout_credit_consumed: bool,
    created_at: String,
    updated_at: String,
    granted_at: Option<String>,
    terminal_at: Option<String>,
}
impl TryFrom<DbRow> for BuildLeaseRow {
    type Error = DbError;
    fn try_from(v: DbRow) -> DbResult<Self> {
        Ok(Self {
            key: BuildLeaseKey {
                consumer_kind: BuildLeaseConsumerKind::parse(&v.consumer_kind)?,
                consumer_id: v.consumer_id,
            },
            immutable_identity: v.immutable_identity,
            enqueue_sequence: v.enqueue_sequence,
            fencing_token: v.fencing_token,
            state: BuildLeaseState::parse(&v.state)?,
            queue_deadline: v.queue_deadline,
            launch_deadline: v.launch_deadline,
            bound_pod_uid: v.bound_pod_uid,
            candidate_cleanup: v.candidate_cleanup.map(|v| v.0),
            terminal_reason: v.terminal_reason,
            timeout_credit_consumed: v.timeout_credit_consumed,
            created_at: v.created_at,
            updated_at: v.updated_at,
            granted_at: v.granted_at,
            terminal_at: v.terminal_at,
        })
    }
}
/// Durable column spelling of a lifecycle state, so occupancy membership is
/// decided against the same [`OCCUPYING`] list the SQL uses.
fn state_str(state: BuildLeaseState) -> &'static str {
    match state {
        BuildLeaseState::Queued => "queued",
        BuildLeaseState::Granted => "granted",
        BuildLeaseState::Launching => "launching",
        BuildLeaseState::Bound => "bound",
        BuildLeaseState::Active => "active",
        BuildLeaseState::Suspect => "suspect",
        BuildLeaseState::Terminal => "terminal",
    }
}
async fn lock(tx: &mut Transaction<'_, Postgres>) -> DbResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('build-lease-ledger',0))")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn fetch(
    tx: &mut Transaction<'_, Postgres>,
    key: &BuildLeaseKey,
    for_update: bool,
) -> DbResult<Option<BuildLeaseRow>> {
    let suffix = if for_update { " FOR UPDATE" } else { "" };
    let row = sqlx::query_as::<_, DbRow>(&format!(
        "SELECT {COLS} FROM build_leases WHERE consumer_kind=$1 AND consumer_id=$2{suffix}"
    ))
    .bind(key.consumer_kind.as_str())
    .bind(&key.consumer_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}
async fn occupancy_tx(tx: &mut Transaction<'_, Postgres>) -> DbResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM build_leases WHERE state=ANY($1)")
            .bind(OCCUPYING.as_slice())
            .fetch_one(&mut **tx)
            .await?,
    )
}
async fn set_cap_tx(tx: &mut Transaction<'_, Postgres>, cap: i64) -> DbResult<()> {
    sqlx::query("UPDATE build_lease_caps SET cap=$1,updated_at=now() WHERE singleton=true")
        .bind(cap)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn expire_queued_tx(tx: &mut Transaction<'_, Postgres>, now: &str) -> DbResult<()> {
    sqlx::query("UPDATE build_leases SET state='terminal',terminal_reason='deadline_expired',terminal_at=$1::timestamptz,updated_at=now() WHERE state='queued' AND queue_deadline <= $1::timestamptz").bind(now).execute(&mut **tx).await?;
    Ok(())
}
async fn snapshot_tx(tx: &mut Transaction<'_, Postgres>) -> DbResult<BuildLeaseSnapshot> {
    let cap = sqlx::query_scalar("SELECT cap FROM build_lease_caps WHERE singleton=true")
        .fetch_one(&mut **tx)
        .await?;
    let occupied = occupancy_tx(tx).await?;
    let rows = sqlx::query_as::<_, DbRow>(&format!(
        "SELECT {COLS} FROM build_leases WHERE state <> 'terminal' ORDER BY enqueue_sequence"
    ))
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect::<DbResult<_>>()?;
    Ok(BuildLeaseSnapshot {
        cap,
        occupied,
        rows,
    })
}
fn validate_cap(cap: i64) -> DbResult<()> {
    if cap < 0 {
        Err(DbError::InvalidData(
            "build lease cap must be non-negative".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const NOW: &str = "2026-01-01T00:00:00Z";
    const LATER: &str = "2026-01-01T01:00:00Z";

    fn input(kind: BuildLeaseConsumerKind, id: &str, identity: &str) -> QueueBuildLeaseInput {
        QueueBuildLeaseInput {
            key: BuildLeaseKey {
                consumer_kind: kind,
                consumer_id: id.into(),
            },
            immutable_identity: identity.into(),
            queue_deadline: None,
            launch_deadline: None,
        }
    }

    async fn grant(repo: &BuildLeaseRepository, cap: i64) -> BuildLeaseRow {
        match repo.grant_next(cap, NOW, Some(LATER)).await.unwrap() {
            GrantNextBuildLeaseResult::Granted(row) => row,
            GrantNextBuildLeaseResult::Empty { .. } => panic!("expected queued lease to grant"),
        }
    }

    #[tokio::test]
    async fn fifo_is_global_across_task_and_graph_warm_consumers() {
        let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());
        let task = input(BuildLeaseConsumerKind::TaskInvocation, "task", "task-v1");
        let warm = input(BuildLeaseConsumerKind::GraphWarm, "warm", "warm-v1");
        repo.queue(&task).await.unwrap();
        repo.queue(&warm).await.unwrap();

        let first = grant(&repo, 1).await;
        assert_eq!(first.key, task.key);
        repo.release(&first.key, first.fencing_token.unwrap(), None)
            .await
            .unwrap();
        let second = grant(&repo, 1).await;
        assert_eq!(second.key, warm.key);
        assert!(first.enqueue_sequence < second.enqueue_sequence);
    }

    #[tokio::test]
    async fn cap_zero_and_cap_reconciliation_preserve_occupied_work() {
        let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());
        let first = input(BuildLeaseConsumerKind::TaskInvocation, "first", "first-v1");
        let second = input(BuildLeaseConsumerKind::GraphWarm, "second", "second-v1");
        repo.queue(&first).await.unwrap();
        repo.queue(&second).await.unwrap();
        assert!(matches!(
            repo.grant_next(0, NOW, None).await.unwrap(),
            GrantNextBuildLeaseResult::Empty {
                occupancy: 0,
                cap: 0
            }
        ));

        let granted = grant(&repo, 1).await;
        let snapshot = repo.set_cap(0).await.unwrap();
        assert_eq!((snapshot.cap, snapshot.occupied), (0, 1));
        assert!(matches!(
            repo.grant_next(2, NOW, None).await.unwrap(),
            GrantNextBuildLeaseResult::Granted(ref row) if row.key == second.key
        ));
        assert_eq!(repo.snapshot().await.unwrap().occupied, 2);
        repo.release(&granted.key, granted.fencing_token.unwrap(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restart_snapshot_orders_nonterminal_rows_by_fifo_sequence() {
        let db = Database::open_in_memory().unwrap();
        let repo = BuildLeaseRepository::new(db.clone());
        let first = input(BuildLeaseConsumerKind::GraphWarm, "first", "first-v1");
        let second = input(
            BuildLeaseConsumerKind::TaskInvocation,
            "second",
            "second-v1",
        );
        repo.queue(&first).await.unwrap();
        repo.queue(&second).await.unwrap();
        let first_row = grant(&repo, 2).await;

        let recovered = BuildLeaseRepository::new(db).snapshot().await.unwrap();
        assert_eq!(recovered.occupied, 1);
        assert_eq!(
            recovered
                .rows
                .iter()
                .map(|row| &row.key)
                .collect::<Vec<_>>(),
            vec![&first.key, &second.key]
        );
        assert_eq!(recovered.rows[0].fencing_token, first_row.fencing_token);
    }

    #[tokio::test]
    async fn queue_replay_pod_binding_and_status_validation_are_idempotent_and_fenced() {
        let repo = BuildLeaseRepository::new(Database::open_in_memory().unwrap());
        let request = input(
            BuildLeaseConsumerKind::TaskInvocation,
            "stable",
            "identity-v1",
        );
        assert!(matches!(
            repo.queue(&request).await.unwrap(),
            QueueBuildLeaseResult::Queued {
                idempotent: false,
                ..
            }
        ));
        assert!(matches!(
            repo.queue(&request).await.unwrap(),
            QueueBuildLeaseResult::Queued {
                idempotent: true,
                ..
            }
        ));
        assert!(matches!(
            repo.queue(&input(
                BuildLeaseConsumerKind::TaskInvocation,
                "stable",
                "identity-v2"
            ))
            .await
            .unwrap(),
            QueueBuildLeaseResult::LeaseIdentityConflict { .. }
        ));

        let granted = grant(&repo, 1).await;
        let token = granted.fencing_token.unwrap();
        assert_eq!(
            repo.status(&request.key, token, BuildLeaseState::Launching, None)
                .await
                .unwrap()
                .state,
            BuildLeaseState::Launching
        );
        // A stale Granted report after Launching is rejected rather than
        // reaching transition's state-to-SQL conversion.
        assert!(matches!(
            repo.status(&request.key, token, BuildLeaseState::Granted, None)
                .await,
            Err(DbError::InvalidData(_))
        ));
        let bound = repo.bind(&request.key, token, "pod-a", None).await.unwrap();
        assert_eq!(bound.bound_pod_uid.as_deref(), Some("pod-a"));
        let delayed_grant = repo
            .status(&request.key, token, BuildLeaseState::Launching, None)
            .await
            .unwrap();
        assert_eq!(delayed_grant.state, BuildLeaseState::Bound);
        assert_eq!(delayed_grant.bound_pod_uid.as_deref(), Some("pod-a"));
        assert_eq!(
            repo.bind(&request.key, token, "pod-a", None)
                .await
                .unwrap()
                .state,
            BuildLeaseState::Bound
        );
        assert!(matches!(
            repo.bind(&request.key, token, "pod-b", None).await,
            Err(DbError::InvalidTransition(_))
        ));

        repo.status(&request.key, token, BuildLeaseState::Active, None)
            .await
            .unwrap();
        assert_eq!(
            repo.status(&request.key, token, BuildLeaseState::Launching, None)
                .await
                .unwrap()
                .state,
            BuildLeaseState::Active
        );
        assert_eq!(
            repo.bind(&request.key, token, "pod-a", None)
                .await
                .unwrap()
                .state,
            BuildLeaseState::Active
        );
        assert!(
            repo.bind(&request.key, token + 1, "pod-a", None)
                .await
                .is_err()
        );
        assert!(repo.bind(&request.key, token, "pod-b", None).await.is_err());
        assert_eq!(
            repo.get(&request.key).await.unwrap().unwrap().state,
            BuildLeaseState::Active
        );

        repo.status(&request.key, token, BuildLeaseState::Suspect, None)
            .await
            .unwrap();
        assert_eq!(
            repo.status(&request.key, token, BuildLeaseState::Launching, None)
                .await
                .unwrap()
                .state,
            BuildLeaseState::Suspect
        );
        assert_eq!(
            repo.bind(&request.key, token, "pod-a", None)
                .await
                .unwrap()
                .state,
            BuildLeaseState::Suspect
        );
        repo.release(&request.key, token, None).await.unwrap();
        let terminal = repo.bind(&request.key, token, "pod-a", None).await.unwrap();
        assert_eq!(terminal.state, BuildLeaseState::Terminal);
        assert_eq!(terminal.terminal_reason.as_deref(), Some("released"));
        let delayed_grant = repo
            .status(&request.key, token, BuildLeaseState::Launching, None)
            .await
            .unwrap();
        assert_eq!(delayed_grant.state, BuildLeaseState::Terminal);
        assert_eq!(delayed_grant.terminal_reason.as_deref(), Some("released"));
        assert!(repo.bind(&request.key, token, "pod-b", None).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_terminal_attempts_return_capacity_exactly_once() {
        let repo = Arc::new(BuildLeaseRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        let first = input(BuildLeaseConsumerKind::TaskInvocation, "first", "first-v1");
        let second = input(BuildLeaseConsumerKind::GraphWarm, "second", "second-v1");
        repo.queue(&first).await.unwrap();
        repo.queue(&second).await.unwrap();
        let granted = grant(&repo, 1).await;
        let token = granted.fencing_token.unwrap();

        let release_repo = Arc::clone(&repo);
        let release_key = first.key.clone();
        let release =
            tokio::spawn(async move { release_repo.release(&release_key, token, None).await });
        let cancel_repo = Arc::clone(&repo);
        let cancel_key = first.key.clone();
        let cancel = tokio::spawn(async move { cancel_repo.cancel(&cancel_key, None).await });
        for result in [release.await.unwrap(), cancel.await.unwrap()] {
            assert_eq!(result.unwrap().state, BuildLeaseState::Terminal);
        }
        assert_eq!(repo.snapshot().await.unwrap().occupied, 0);
        let next = grant(&repo, 1).await;
        assert_eq!(next.key, second.key);
        assert_eq!(repo.snapshot().await.unwrap().occupied, 1);
    }
}
