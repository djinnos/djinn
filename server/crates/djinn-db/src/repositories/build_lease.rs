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
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLeaseRow {
    pub key: BuildLeaseKey,
    pub immutable_identity: String,
    pub enqueue_sequence: i64,
    pub fencing_token: Option<i64>,
    pub state: BuildLeaseState,
    pub queue_deadline: Option<String>,
    pub launch_deadline: Option<String>,
    pub bound_pod_uid: Option<String>,
    pub candidate_cleanup: Option<serde_json::Value>,
    pub terminal_reason: Option<String>,
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
        let row = sqlx::query_as::<_, DbRow>(&format!("INSERT INTO build_leases (consumer_kind,consumer_id,immutable_identity,queue_deadline,state) VALUES ($1,$2,$3,$4::timestamptz,'queued') RETURNING {COLS}"))
            .bind(input.key.consumer_kind.as_str()).bind(&input.key.consumer_id).bind(&input.immutable_identity).bind(&input.queue_deadline).fetch_one(&mut *tx).await?;
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
        let row = sqlx::query_as::<_, DbRow>(&format!("UPDATE build_leases SET state='granted', fencing_token=nextval('build_lease_fencing_token_seq'), granted_at=$1::timestamptz, launch_deadline=$2::timestamptz, updated_at=now() WHERE consumer_kind=$3 AND consumer_id=$4 RETURNING {COLS}"))
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

    /// Status reports are idempotent, token-fenced and cannot resurrect terminal work.
    pub async fn status(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        state: BuildLeaseState,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        if matches!(state, BuildLeaseState::Queued | BuildLeaseState::Terminal) {
            return Err(DbError::InvalidData(
                "status must be an occupied lifecycle state".into(),
            ));
        }
        self.transition(
            key,
            token,
            &[
                BuildLeaseState::Granted,
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
                BuildLeaseState::Active,
                BuildLeaseState::Suspect,
            ],
            state,
            None,
            cleanup,
        )
        .await
    }
    pub async fn abandon(
        &self,
        key: &BuildLeaseKey,
        token: i64,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(key, Some(token), "abandoned", cleanup).await
    }
    pub async fn cancel(
        &self,
        key: &BuildLeaseKey,
        cleanup: Option<serde_json::Value>,
    ) -> DbResult<BuildLeaseRow> {
        self.terminal(key, None, "cancelled", cleanup).await
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
        self.transition(
            key,
            token,
            &[
                BuildLeaseState::Granted,
                BuildLeaseState::Launching,
                BuildLeaseState::Bound,
            ],
            BuildLeaseState::Bound,
            Some(pod_uid),
            cleanup,
        )
        .await
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
        if row.state == BuildLeaseState::Terminal {
            tx.commit().await?;
            return Ok(row);
        }
        if let Some(token) = token
            && row.fencing_token != Some(token)
        {
            return Err(DbError::InvalidTransition(
                "stale build lease fencing token".into(),
            ));
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
        if row.state == state && (pod_uid.is_none() || row.bound_pod_uid.as_deref() == pod_uid) {
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

const COLS: &str = "consumer_kind,consumer_id,immutable_identity,enqueue_sequence,fencing_token,state,queue_deadline::text,launch_deadline::text,bound_pod_uid,candidate_cleanup,terminal_reason,created_at::text,updated_at::text,granted_at::text,terminal_at::text";
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
            created_at: v.created_at,
            updated_at: v.updated_at,
            granted_at: v.granted_at,
            terminal_at: v.terminal_at,
        })
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
