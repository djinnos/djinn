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
