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
    released_fencing_token, release_reason, pod_namespace, pod_name, pod_uid, launcher_container_name, launcher_container_id, image_digest, observed_launcher_protocol, effective_launcher_protocol, admitted_cpu_millicores";

/// Durable lifecycle states defined by migration 162.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildPodPermitState {
    Acquired,
    JobCreated,
    BirthConfirmed,
    LiftApplying,
    Lifted,
    DropRequired,
    DropApplying,
    Quarantined,
    Released,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureBuildPodResizeIdentityResult {
    Captured(BuildPodPermitRow),
    AlreadyCaptured(BuildPodPermitRow),
    Rejected,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionBuildPodResizeLifecycleResult {
    Transitioned(BuildPodPermitRow),
    Rejected,
}

impl BuildPodPermitState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::JobCreated => "job_created",
            Self::BirthConfirmed => "birth_confirmed",
            Self::LiftApplying => "lift_applying",
            Self::Lifted => "lifted",
            Self::DropRequired => "drop_required",
            Self::DropApplying => "drop_applying",
            Self::Quarantined => "quarantined",
            Self::Released => "released",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildPodResizeIdentity {
    pub pod_namespace: String,
    pub pod_name: String,
    pub pod_uid: String,
    pub launcher_container_name: String,
    pub launcher_container_id: String,
    pub image_digest: String,
    pub observed_launcher_protocol: String,
    pub effective_launcher_protocol: String,
    pub admitted_cpu_millicores: i64,
}
impl BuildPodResizeIdentity {
    fn valid(&self) -> bool {
        !self.pod_namespace.trim().is_empty()
            && !self.pod_name.trim().is_empty()
            && !self.pod_uid.trim().is_empty()
            && !self.launcher_container_name.trim().is_empty()
            && !self.launcher_container_id.trim().is_empty()
            && !self.image_digest.trim().is_empty()
            && matches!(
                self.observed_launcher_protocol.as_str(),
                "leaf-v1" | "resize-v2"
            )
            && matches!(
                self.effective_launcher_protocol.as_str(),
                "leaf-v1" | "resize-v2"
            )
            && self.admitted_cpu_millicores > 0
    }
}

impl BuildPodPermitState {
    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "acquired" => Ok(Self::Acquired),
            "job_created" => Ok(Self::JobCreated),
            "birth_confirmed" => Ok(Self::BirthConfirmed),
            "lift_applying" => Ok(Self::LiftApplying),
            "lifted" => Ok(Self::Lifted),
            "drop_required" => Ok(Self::DropRequired),
            "drop_applying" => Ok(Self::DropApplying),
            "quarantined" => Ok(Self::Quarantined),
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
    pub resize_identity: Option<BuildPodResizeIdentity>,
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

    /// Verify that the canonical global pool row can be read without acquiring
    /// or otherwise mutating a permit. A missing relation is intentionally an
    /// error, while a missing singleton row is a successful `false` result.
    pub async fn global_pool_is_readable(&self) -> DbResult<bool> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM build_pod_permit_pools WHERE pool_key = $1)",
        )
        .bind(POOL_KEY)
        .fetch_one(self.db.pool())
        .await?)
    }

    /// Remove the canonical pool singleton for prerequisite-gate fixtures.
    ///
    /// This test-only seam keeps callers outside `djinn-db` from bypassing the
    /// repository raw-SQL boundary merely to simulate a partial migration.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn delete_global_pool_for_test(&self) -> DbResult<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM build_pod_permit_pools WHERE pool_key = $1")
            .bind(POOL_KEY)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Drop the pool relation for prerequisite-gate error fixtures.
    ///
    /// This is deliberately test-support-only: production callers can only
    /// observe the repository error through [`Self::global_pool_is_readable`].
    #[cfg(any(test, feature = "test-support"))]
    pub async fn drop_pool_relation_for_test(&self) -> DbResult<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DROP TABLE build_pod_permit_pools")
            .execute(self.db.pool())
            .await?;
        Ok(())
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

    pub async fn capture_resize_identity(
        &self,
        task_run_id: &str,
        permit_id: &str,
        fencing_token: i64,
        identity: &BuildPodResizeIdentity,
    ) -> DbResult<CaptureBuildPodResizeIdentityResult> {
        if !identity.valid() {
            return Ok(CaptureBuildPodResizeIdentityResult::Rejected);
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let updated = sqlx::query_as::<_, DbRow>(&format!("UPDATE build_pod_permits SET state='birth_confirmed', pod_namespace=$1, pod_name=$2, pod_uid=$3, launcher_container_name=$4, launcher_container_id=$5, image_digest=$6, observed_launcher_protocol=$7, effective_launcher_protocol=$8, admitted_cpu_millicores=$9 WHERE task_run_id=$10 AND permit_id=$11::uuid AND fencing_token=$12 AND state='job_created' AND pod_uid IS NULL RETURNING {ROW_COLUMNS}"))
            .bind(&identity.pod_namespace).bind(&identity.pod_name).bind(&identity.pod_uid).bind(&identity.launcher_container_name).bind(&identity.launcher_container_id).bind(&identity.image_digest).bind(&identity.observed_launcher_protocol).bind(&identity.effective_launcher_protocol).bind(identity.admitted_cpu_millicores).bind(task_run_id).bind(permit_id).bind(fencing_token).fetch_optional(&mut *tx).await?;
        if let Some(row) = updated {
            tx.commit().await?;
            return Ok(CaptureBuildPodResizeIdentityResult::Captured(
                row.try_into()?,
            ));
        }
        let existing = fetch_tx(&mut tx, task_run_id).await?;
        tx.commit().await?;
        Ok(match existing {
            Some(row)
                if row.permit_id == permit_id
                    && row.fencing_token == fencing_token
                    && row.resize_identity.as_ref() == Some(identity) =>
            {
                CaptureBuildPodResizeIdentityResult::AlreadyCaptured(row)
            }
            _ => CaptureBuildPodResizeIdentityResult::Rejected,
        })
    }

    pub async fn transition_resize_lifecycle(
        &self,
        task_run_id: &str,
        permit_id: &str,
        fencing_token: i64,
        pod_uid: &str,
        expected: BuildPodPermitState,
        next: BuildPodPermitState,
    ) -> DbResult<TransitionBuildPodResizeLifecycleResult> {
        if pod_uid.trim().is_empty() {
            return Ok(TransitionBuildPodResizeLifecycleResult::Rejected);
        }
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, DbRow>(&format!("UPDATE build_pod_permits SET state=$1 WHERE task_run_id=$2 AND permit_id=$3::uuid AND fencing_token=$4 AND pod_uid=$5 AND state=$6 RETURNING {ROW_COLUMNS}"))
            .bind(next.as_str()).bind(task_run_id).bind(permit_id).bind(fencing_token).bind(pod_uid).bind(expected.as_str()).fetch_optional(self.db.pool()).await?;
        Ok(match row {
            Some(row) => TransitionBuildPodResizeLifecycleResult::Transitioned(row.try_into()?),
            None => TransitionBuildPodResizeLifecycleResult::Rejected,
        })
    }

    pub async fn list_nonterminal_resize(&self) -> DbResult<Vec<BuildPodPermitRow>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as::<_, DbRow>(&format!("SELECT {ROW_COLUMNS} FROM build_pod_permits WHERE state IN ('birth_confirmed','lift_applying','lifted','drop_required','drop_applying','quarantined') ORDER BY acquired_at, task_run_id")).fetch_all(self.db.pool()).await?.into_iter().map(TryInto::try_into).collect()
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
               AND state <> 'released' RETURNING {ROW_COLUMNS}"
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
    pod_namespace: Option<String>,
    pod_name: Option<String>,
    pod_uid: Option<String>,
    launcher_container_name: Option<String>,
    launcher_container_id: Option<String>,
    image_digest: Option<String>,
    observed_launcher_protocol: Option<String>,
    effective_launcher_protocol: Option<String>,
    admitted_cpu_millicores: Option<i64>,
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
            resize_identity: match (
                row.pod_namespace,
                row.pod_name,
                row.pod_uid,
                row.launcher_container_name,
                row.launcher_container_id,
                row.image_digest,
                row.observed_launcher_protocol,
                row.effective_launcher_protocol,
                row.admitted_cpu_millicores,
            ) {
                (
                    Some(pod_namespace),
                    Some(pod_name),
                    Some(pod_uid),
                    Some(launcher_container_name),
                    Some(launcher_container_id),
                    Some(image_digest),
                    Some(observed_launcher_protocol),
                    Some(effective_launcher_protocol),
                    Some(admitted_cpu_millicores),
                ) => Some(BuildPodResizeIdentity {
                    pod_namespace,
                    pod_name,
                    pod_uid,
                    launcher_container_name,
                    launcher_container_id,
                    image_digest,
                    observed_launcher_protocol,
                    effective_launcher_protocol,
                    admitted_cpu_millicores,
                }),
                (None, None, None, None, None, None, None, None, None) => None,
                _ => {
                    return Err(DbError::InvalidData(
                        "partial build pod resize identity".into(),
                    ));
                }
            },
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
