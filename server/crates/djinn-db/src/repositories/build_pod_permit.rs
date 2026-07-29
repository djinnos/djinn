//! Durable PostgreSQL build-pod permit admission.
//!
//! Capacity is deliberately serialized by the singleton `global` pool row from
//! migration 162. A missing pool/table or any database error is an unavailable
//! outcome, never an admission decision.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

const POOL_KEY: &str = "global";
const ROW_COLUMNS: &str = "task_run_id, permit_id::text AS permit_id, fencing_token, state, job_uid, \
    to_char(acquired_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS acquired_at, \
    to_char(released_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS released_at, \
    released_fencing_token, release_reason";

/// Durable lifecycle states defined by migration 162.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildPodPermitState {
    Acquired,
    JobCreated,
    Released,
}

impl BuildPodPermitState {
    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "acquired" => Ok(Self::Acquired),
            "job_created" => Ok(Self::JobCreated),
            "released" => Ok(Self::Released),
            _ => Err(DbError::InvalidData(format!(
                "invalid build pod permit state `{value}`"
            ))),
        }
    }

    fn is_active(self) -> bool {
        self != Self::Released
    }
}

/// Typed durable permit row. `permit_id` and `fencing_token` are immutable
/// identities and must be echoed to bind or release a permit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPodPermitRow {
    pub task_run_id: String,
    pub permit_id: String,
    pub fencing_token: i64,
    pub state: BuildPodPermitState,
    pub job_uid: Option<String>,
    pub acquired_at: String,
    pub released_at: Option<String>,
    pub released_fencing_token: Option<i64>,
    pub release_reason: Option<String>,
}

/// The admission result deliberately has no successful fallback for unavailable
/// durable storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcquireBuildPodPermitResult {
    Acquired {
        row: BuildPodPermitRow,
        idempotent: bool,
    },
    PoolFull {
        active_count: i64,
        limit: i64,
    },
    /// A task run has one durable permit lifecycle and a released permit is not
    /// resurrected under its old identity.
    AlreadyReleased {
        row: BuildPodPermitRow,
    },
    InvalidLimit {
        limit: i64,
    },
    Unavailable,
}

/// The result of binding an observed Job UID. The schema makes a present UID
/// immutable, so a different UID is rejected without a write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindBuildPodPermitResult {
    Bound(BuildPodPermitRow),
    AlreadyBound(BuildPodPermitRow),
    Rejected,
}

/// Fenced release result. A matching replay is explicitly idempotent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseBuildPodPermitResult {
    Released(BuildPodPermitRow),
    AlreadyReleased(BuildPodPermitRow),
    Rejected,
}

/// Repository for the migration-162 permit lifecycle.
pub struct BuildPodPermitRepository {
    db: Database,
}

impl BuildPodPermitRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Atomically acquire one permit below `limit`.
    ///
    /// The `global` pool row is locked before the task-run lookup/count/insert.
    /// Therefore two contenders cannot both observe the final unit. Errors are
    /// intentionally collapsed into `Unavailable`, making this API fail closed.
    pub async fn acquire(&self, task_run_id: &str, limit: i64) -> AcquireBuildPodPermitResult {
        if limit <= 0 || task_run_id.trim().is_empty() {
            return AcquireBuildPodPermitResult::InvalidLimit { limit };
        }
        match self.acquire_inner(task_run_id, limit).await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(error = %error, "build pod permit acquisition unavailable");
                AcquireBuildPodPermitResult::Unavailable
            }
        }
    }

    async fn acquire_inner(
        &self,
        task_run_id: &str,
        limit: i64,
    ) -> DbResult<AcquireBuildPodPermitResult> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let pool: Option<String> = sqlx::query_scalar(
            "SELECT pool_key FROM build_pod_permit_pools WHERE pool_key = $1 FOR UPDATE",
        )
        .bind(POOL_KEY)
        .fetch_optional(&mut *tx)
        .await?;
        if pool.is_none() {
            return Err(DbError::Internal(
                "build pod permit global pool is missing".into(),
            ));
        }

        if let Some(row) = fetch_tx(&mut tx, task_run_id).await? {
            tx.commit().await?;
            return Ok(if row.state.is_active() {
                AcquireBuildPodPermitResult::Acquired {
                    row,
                    idempotent: true,
                }
            } else {
                // A task run has one immutable permit lifecycle. A released row
                // cannot be resurrected with a new fence under the same run id.
                AcquireBuildPodPermitResult::AlreadyReleased { row }
            });
        }

        let active_count = active_count_tx(&mut tx).await?;
        if active_count >= limit {
            tx.commit().await?;
            return Ok(AcquireBuildPodPermitResult::PoolFull {
                active_count,
                limit,
            });
        }
        let row = sqlx::query_as::<_, DbRow>(&format!(
            "INSERT INTO build_pod_permits (task_run_id) VALUES ($1) RETURNING {ROW_COLUMNS}"
        ))
        .bind(task_run_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AcquireBuildPodPermitResult::Acquired {
            row: row.try_into()?,
            idempotent: false,
        })
    }

    /// Return a row only when it remains capacity-active.
    pub async fn active(&self, task_run_id: &str) -> DbResult<Option<BuildPodPermitRow>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, DbRow>(&format!(
            "SELECT {ROW_COLUMNS} FROM build_pod_permits WHERE task_run_id = $1 AND state <> 'released'"
        ))
        .bind(task_run_id)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// List every active permit. Terminal Jobs remain present until release.
    pub async fn list_active(&self) -> DbResult<Vec<BuildPodPermitRow>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as::<_, DbRow>(&format!(
            "SELECT {ROW_COLUMNS} FROM build_pod_permits WHERE state <> 'released' ORDER BY acquired_at, task_run_id"
        ))
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    pub async fn active_count(&self) -> DbResult<i64> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar(
            "SELECT count(*)::bigint FROM build_pod_permits WHERE state <> 'released'",
        )
        .fetch_one(self.db.pool())
        .await?)
    }

    /// Bind an observed Kubernetes Job UID, or refresh the matching observation.
    /// The conditional predicate fences task run, permit identity, token, and
    /// current state in the same update.
    pub async fn bind_or_refresh_job_uid(
        &self,
        task_run_id: &str,
        permit_id: &str,
        fencing_token: i64,
        job_uid: &str,
    ) -> DbResult<BindBuildPodPermitResult> {
        if job_uid.trim().is_empty() {
            return Ok(BindBuildPodPermitResult::Rejected);
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let updated = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_pod_permits SET state = 'job_created', job_uid = $1 \
             WHERE task_run_id = $2 AND permit_id = $3::uuid AND fencing_token = $4 \
               AND state = 'acquired' AND job_uid IS NULL RETURNING {ROW_COLUMNS}"
        ))
        .bind(job_uid)
        .bind(task_run_id)
        .bind(permit_id)
        .bind(fencing_token)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = updated {
            tx.commit().await?;
            return Ok(BindBuildPodPermitResult::Bound(row.try_into()?));
        }
        let existing = fetch_tx(&mut tx, task_run_id).await?;
        tx.commit().await?;
        Ok(match existing {
            Some(row)
                if row.permit_id == permit_id
                    && row.fencing_token == fencing_token
                    && row.state == BuildPodPermitState::JobCreated
                    && row.job_uid.as_deref() == Some(job_uid) =>
            {
                BindBuildPodPermitResult::AlreadyBound(row)
            }
            _ => BindBuildPodPermitResult::Rejected,
        })
    }

    /// Fenced explicit release. Released rows are retained to reject stale
    /// owners while allowing the exact release replay to be idempotent.
    pub async fn release(
        &self,
        task_run_id: &str,
        permit_id: &str,
        fencing_token: i64,
        reason: &str,
    ) -> DbResult<ReleaseBuildPodPermitResult> {
        if reason.trim().is_empty() || reason.len() > 64 {
            return Ok(ReleaseBuildPodPermitResult::Rejected);
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let updated = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_pod_permits SET state = 'released', released_at = now(), \
             released_fencing_token = fencing_token, release_reason = $1 \
             WHERE task_run_id = $2 AND permit_id = $3::uuid AND fencing_token = $4 \
               AND state IN ('acquired', 'job_created') RETURNING {ROW_COLUMNS}"
        ))
        .bind(reason)
        .bind(task_run_id)
        .bind(permit_id)
        .bind(fencing_token)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = updated {
            tx.commit().await?;
            return Ok(ReleaseBuildPodPermitResult::Released(row.try_into()?));
        }
        let existing = fetch_tx(&mut tx, task_run_id).await?;
        tx.commit().await?;
        Ok(match existing {
            Some(row)
                if row.permit_id == permit_id
                    && row.fencing_token == fencing_token
                    && row.state == BuildPodPermitState::Released =>
            {
                ReleaseBuildPodPermitResult::AlreadyReleased(row)
            }
            _ => ReleaseBuildPodPermitResult::Rejected,
        })
    }
}

#[derive(sqlx::FromRow)]
struct DbRow {
    task_run_id: String,
    permit_id: String,
    fencing_token: i64,
    state: String,
    job_uid: Option<String>,
    acquired_at: String,
    released_at: Option<String>,
    released_fencing_token: Option<i64>,
    release_reason: Option<String>,
}

impl TryFrom<DbRow> for BuildPodPermitRow {
    type Error = DbError;

    fn try_from(row: DbRow) -> DbResult<Self> {
        Ok(Self {
            task_run_id: row.task_run_id,
            permit_id: row.permit_id,
            fencing_token: row.fencing_token,
            state: BuildPodPermitState::parse(&row.state)?,
            job_uid: row.job_uid,
            acquired_at: row.acquired_at,
            released_at: row.released_at,
            released_fencing_token: row.released_fencing_token,
            release_reason: row.release_reason,
        })
    }
}

async fn fetch_tx(
    tx: &mut Transaction<'_, Postgres>,
    task_run_id: &str,
) -> DbResult<Option<BuildPodPermitRow>> {
    let row = sqlx::query_as::<_, DbRow>(&format!(
        "SELECT {ROW_COLUMNS} FROM build_pod_permits WHERE task_run_id = $1"
    ))
    .bind(task_run_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}

async fn active_count_tx(tx: &mut Transaction<'_, Postgres>) -> DbResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*)::bigint FROM build_pod_permits WHERE state <> 'released'",
    )
    .fetch_one(&mut **tx)
    .await?)
}
