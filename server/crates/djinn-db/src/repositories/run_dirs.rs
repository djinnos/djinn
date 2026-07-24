//! Durable run-dir ledger primitives for disk-aware build admission.
//!
//! This repository owns storage invariants and compare-and-set generation
//! transitions for per-pod Cargo run directories. Admission policy, capacity
//! observation, seeding, reconciliation orchestration, and any deletion decision
//! remain in higher layers. Nothing here creates, seeds, or deletes a filesystem
//! directory — the ledger only records the durable lifecycle state.
//!
//! Ships DARK/OBSERVE (proposal nquz, phase 1): no production caller writes rows
//! through this ledger yet, and no automated GC path consumes it.
//!
//! Serialization mirrors `admission_journal.rs`: every mutating transition takes
//! a per-volume transaction-scoped advisory lock plus `SELECT ... FOR UPDATE` on
//! the run-dir row, then applies a compare-and-set on `(state, generation)`.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

const RUN_DIR_COLUMNS: &str = "volume_id, pod_uid, task_run_id, project_id, base_fingerprint, \
     state, generation, reserved_bytes, measured_bytes, quota_id, \
     last_lease_at::text, temp_path, final_path, created_at::text, updated_at::text";

/// Durable lease-coupled lifecycle state of a run directory.
///
/// The eight lifecycle states follow the proposal state table. The extra
/// [`RunDirState::QuarantinedUnowned`] bucket is reconciliation-only: it counts
/// against observed physical bytes but is never an automated deletion candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDirState {
    /// No published or temporary directory.
    Absent,
    /// Bytes and quota ID committed; no allocation yet.
    Reserved,
    /// One generation owns a temp path.
    Seeding,
    /// Published and protected by a current/recent lease.
    ReadyActive,
    /// Live pod, lease-recency window elapsed.
    ReadyIdle,
    /// Terminal pod proof landed; eligible for reclaim.
    Reclaimable,
    /// One GC generation owns deletion.
    Reclaiming,
    /// Reconciliation could not resolve authoritative ownership.
    QuarantinedUnowned,
}

impl RunDirState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Reserved => "reserved",
            Self::Seeding => "seeding",
            Self::ReadyActive => "ready_active",
            Self::ReadyIdle => "ready_idle",
            Self::Reclaimable => "reclaimable",
            Self::Reclaiming => "reclaiming",
            Self::QuarantinedUnowned => "quarantined_unowned",
        }
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "absent" => Ok(Self::Absent),
            "reserved" => Ok(Self::Reserved),
            "seeding" => Ok(Self::Seeding),
            "ready_active" => Ok(Self::ReadyActive),
            "ready_idle" => Ok(Self::ReadyIdle),
            "reclaimable" => Ok(Self::Reclaimable),
            "reclaiming" => Ok(Self::Reclaiming),
            "quarantined_unowned" => Ok(Self::QuarantinedUnowned),
            _ => Err(DbError::InvalidData(format!(
                "invalid run-dir state `{value}`"
            ))),
        }
    }

    /// Every state string, for bounded telemetry rollups.
    pub const ALL: [RunDirState; 8] = [
        Self::Absent,
        Self::Reserved,
        Self::Seeding,
        Self::ReadyActive,
        Self::ReadyIdle,
        Self::Reclaimable,
        Self::Reclaiming,
        Self::QuarantinedUnowned,
    ];
}

/// Identity of one run directory on a node-local cache volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDirKey {
    pub volume_id: String,
    pub pod_uid: String,
}

/// Input required to reserve one run-dir generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveRunDirInput {
    pub key: RunDirKey,
    pub task_run_id: Option<String>,
    pub project_id: Option<String>,
    pub base_fingerprint: Option<String>,
    pub reserved_bytes: i64,
    pub quota_id: Option<String>,
}

/// Authoritative evidence used to insert a reconciled row at startup.
///
/// Reconciliation never deletes: an unresolved or malformed directory is
/// recorded as [`RunDirState::QuarantinedUnowned`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciledRunDirInput {
    pub key: RunDirKey,
    pub task_run_id: Option<String>,
    pub project_id: Option<String>,
    pub base_fingerprint: Option<String>,
    pub state: RunDirState,
    pub measured_bytes: i64,
    pub final_path: Option<String>,
}

/// Durable ledger record. Timestamps are RFC3339-formatted by PostgreSQL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDirRow {
    pub key: RunDirKey,
    pub task_run_id: Option<String>,
    pub project_id: Option<String>,
    pub base_fingerprint: Option<String>,
    pub state: RunDirState,
    pub generation: i64,
    pub reserved_bytes: i64,
    pub measured_bytes: i64,
    pub quota_id: Option<String>,
    pub last_lease_at: Option<String>,
    pub temp_path: Option<String>,
    pub final_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Aggregated per-state totals for one volume; drives bounded telemetry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDirStateTotals {
    pub state: RunDirState,
    pub count: i64,
    pub reserved_bytes: i64,
    pub measured_bytes: i64,
}

/// Atomic Postgres repository for the run-dir ledger.
pub struct RunDirRepository {
    db: Database,
}

impl RunDirRepository {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Reserve a new generation for a run directory.
    ///
    /// A missing or `absent` row transitions to `reserved` with an incremented
    /// generation so a stale later callback for the prior generation can never
    /// match. Any non-absent existing state is a conflict and fails closed.
    /// Re-reserving an already-`reserved` row with the same identity is
    /// idempotent and returns the current row unchanged.
    pub async fn reserve(&self, input: &ReserveRunDirInput) -> DbResult<RunDirRow> {
        if input.reserved_bytes < 0 {
            return Err(DbError::InvalidData(
                "run-dir reserved_bytes must be non-negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_volume(&mut tx, &input.key.volume_id).await?;

        let existing = fetch_for_update(&mut tx, &input.key).await?;
        let row = match existing {
            None => {
                let inserted = sqlx::query_as::<_, RunDirDbRow>(&format!(
                    "INSERT INTO run_dirs \
                     (volume_id, pod_uid, task_run_id, project_id, base_fingerprint, \
                      state, generation, reserved_bytes, measured_bytes, quota_id) \
                     VALUES ($1, $2, $3, $4, $5, 'reserved', 0, $6, 0, $7) \
                     RETURNING {RUN_DIR_COLUMNS}"
                ))
                .bind(&input.key.volume_id)
                .bind(&input.key.pod_uid)
                .bind(&input.task_run_id)
                .bind(&input.project_id)
                .bind(&input.base_fingerprint)
                .bind(input.reserved_bytes)
                .bind(&input.quota_id)
                .fetch_one(&mut *tx)
                .await?;
                inserted.try_into()?
            }
            Some(current) if current.state == RunDirState::Reserved => current,
            Some(current) if current.state == RunDirState::Absent => {
                let updated = sqlx::query_as::<_, RunDirDbRow>(&format!(
                    "UPDATE run_dirs SET state = 'reserved', generation = generation + 1, \
                     task_run_id = $3, project_id = $4, base_fingerprint = $5, \
                     reserved_bytes = $6, measured_bytes = 0, quota_id = $7, \
                     temp_path = NULL, final_path = NULL, updated_at = now() \
                     WHERE volume_id = $1 AND pod_uid = $2 RETURNING {RUN_DIR_COLUMNS}"
                ))
                .bind(&input.key.volume_id)
                .bind(&input.key.pod_uid)
                .bind(&input.task_run_id)
                .bind(&input.project_id)
                .bind(&input.base_fingerprint)
                .bind(input.reserved_bytes)
                .bind(&input.quota_id)
                .fetch_one(&mut *tx)
                .await?;
                updated.try_into()?
            }
            Some(current) => {
                return Err(DbError::InvalidTransition(format!(
                    "cannot reserve run-dir from {:?}",
                    current.state
                )));
            }
        };
        tx.commit().await?;
        Ok(row)
    }

    /// Compare-and-set a run-dir transition.
    ///
    /// The transition proceeds only when the current row exists with
    /// `generation == expected_generation` and `state ∈ allowed_from`. It is the
    /// linearization point: exactly one of a competing acquire and GC wins.
    async fn transition(
        &self,
        key: &RunDirKey,
        expected_generation: i64,
        allowed_from: &[RunDirState],
        spec: TransitionSpec<'_>,
    ) -> DbResult<RunDirRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_volume(&mut tx, &key.volume_id).await?;

        let current = fetch_for_update(&mut tx, key)
            .await?
            .ok_or_else(|| DbError::InvalidTransition(format!("run-dir {key:?} does not exist")))?;
        if current.generation != expected_generation {
            return Err(DbError::InvalidTransition(format!(
                "stale run-dir generation {} (durable {})",
                expected_generation, current.generation
            )));
        }
        // A same-target no-op re-application is idempotent within the generation.
        if current.state == spec.to && !spec.bump_generation {
            return Ok(current);
        }
        if !allowed_from.contains(&current.state) {
            return Err(DbError::InvalidTransition(format!(
                "cannot transition run-dir to {:?} from {:?}",
                spec.to, current.state
            )));
        }
        let gen_expr = if spec.bump_generation {
            "generation + 1"
        } else {
            "generation"
        };
        let sql = format!(
            "UPDATE run_dirs SET state = $3, generation = {gen_expr}, \
             temp_path = CASE WHEN $4 THEN $5 ELSE temp_path END, \
             final_path = CASE WHEN $6 THEN $7 ELSE final_path END, \
             measured_bytes = CASE WHEN $8 THEN $9 ELSE measured_bytes END, \
             reserved_bytes = CASE WHEN $10 THEN 0 ELSE reserved_bytes END, \
             quota_id = CASE WHEN $10 THEN NULL ELSE quota_id END, \
             last_lease_at = CASE WHEN $11 THEN now() ELSE last_lease_at END, \
             updated_at = now() \
             WHERE volume_id = $1 AND pod_uid = $2 RETURNING {RUN_DIR_COLUMNS}"
        );
        let updated = sqlx::query_as::<_, RunDirDbRow>(&sql)
            .bind(&key.volume_id)
            .bind(&key.pod_uid)
            .bind(spec.to.as_str())
            .bind(spec.temp_path.is_some())
            .bind(spec.temp_path)
            .bind(spec.final_path.is_some())
            .bind(spec.final_path)
            .bind(spec.measured_bytes.is_some())
            .bind(spec.measured_bytes.unwrap_or(0))
            .bind(spec.release_reservation)
            .bind(spec.touch_lease)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        updated.try_into()
    }

    /// `reserved` → `seeding`: one generation takes ownership of a temp path.
    pub async fn mark_seeding(
        &self,
        key: &RunDirKey,
        generation: i64,
        temp_path: &str,
    ) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::Reserved],
            TransitionSpec {
                to: RunDirState::Seeding,
                temp_path: Some(temp_path),
                ..TransitionSpec::default_for(RunDirState::Seeding)
            },
        )
        .await
    }

    /// `seeding` → `ready_active`: publication landed with measured bytes.
    pub async fn mark_ready_active(
        &self,
        key: &RunDirKey,
        generation: i64,
        final_path: &str,
        measured_bytes: i64,
    ) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::Seeding],
            TransitionSpec {
                to: RunDirState::ReadyActive,
                final_path: Some(final_path),
                measured_bytes: Some(measured_bytes),
                touch_lease: true,
                ..TransitionSpec::default_for(RunDirState::ReadyActive)
            },
        )
        .await
    }

    /// `ready_idle` (or `ready_active`) → `ready_active`: a fresh lease.
    pub async fn touch_lease(&self, key: &RunDirKey, generation: i64) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::ReadyIdle, RunDirState::ReadyActive],
            TransitionSpec {
                to: RunDirState::ReadyActive,
                touch_lease: true,
                ..TransitionSpec::default_for(RunDirState::ReadyActive)
            },
        )
        .await
    }

    /// `ready_active` → `ready_idle`: the lease-recency window elapsed.
    pub async fn mark_ready_idle(&self, key: &RunDirKey, generation: i64) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::ReadyActive],
            TransitionSpec::default_for(RunDirState::ReadyIdle),
        )
        .await
    }

    /// `ready_active` | `ready_idle` → `reclaimable`: terminal pod proof landed.
    pub async fn mark_reclaimable(&self, key: &RunDirKey, generation: i64) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::ReadyActive, RunDirState::ReadyIdle],
            TransitionSpec::default_for(RunDirState::Reclaimable),
        )
        .await
    }

    /// `ready_idle` | `reclaimable` → `reclaiming`: a GC generation owns deletion.
    ///
    /// The generation is bumped so a later acquire that raced this selection
    /// fails its own compare-and-set.
    pub async fn mark_reclaiming(&self, key: &RunDirKey, generation: i64) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::ReadyIdle, RunDirState::Reclaimable],
            TransitionSpec {
                bump_generation: true,
                ..TransitionSpec::default_for(RunDirState::Reclaiming)
            },
        )
        .await
    }

    /// `reclaiming` → `absent`: deletion committed; reservation/quota released.
    pub async fn mark_absent_after_reclaim(
        &self,
        key: &RunDirKey,
        generation: i64,
    ) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::Reclaiming],
            TransitionSpec {
                release_reservation: true,
                ..TransitionSpec::default_for(RunDirState::Absent)
            },
        )
        .await
    }

    /// `reserved` | `seeding` → `absent`: recovery released a partial generation.
    pub async fn release_reservation(
        &self,
        key: &RunDirKey,
        generation: i64,
    ) -> DbResult<RunDirRow> {
        self.transition(
            key,
            generation,
            &[RunDirState::Reserved, RunDirState::Seeding],
            TransitionSpec {
                release_reservation: true,
                ..TransitionSpec::default_for(RunDirState::Absent)
            },
        )
        .await
    }

    /// Insert a reconciled row from authoritative inventory evidence.
    ///
    /// Idempotent: a pre-existing `(volume_id, pod_uid)` row is left untouched
    /// and returned as-is, so re-running reconciliation never overwrites a live
    /// lifecycle row.
    pub async fn upsert_reconciled(&self, input: &ReconciledRunDirInput) -> DbResult<RunDirRow> {
        if input.measured_bytes < 0 {
            return Err(DbError::InvalidData(
                "run-dir measured_bytes must be non-negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_volume(&mut tx, &input.key.volume_id).await?;
        if let Some(existing) = fetch_for_update(&mut tx, &input.key).await? {
            tx.commit().await?;
            return Ok(existing);
        }
        let inserted = sqlx::query_as::<_, RunDirDbRow>(&format!(
            "INSERT INTO run_dirs \
             (volume_id, pod_uid, task_run_id, project_id, base_fingerprint, \
              state, generation, reserved_bytes, measured_bytes, quota_id, \
              last_lease_at, final_path) \
             VALUES ($1, $2, $3, $4, $5, $6, 0, 0, $7, NULL, \
                     CASE WHEN $6 IN ('ready_active','ready_idle') THEN now() ELSE NULL END, $8) \
             RETURNING {RUN_DIR_COLUMNS}"
        ))
        .bind(&input.key.volume_id)
        .bind(&input.key.pod_uid)
        .bind(&input.task_run_id)
        .bind(&input.project_id)
        .bind(&input.base_fingerprint)
        .bind(input.state.as_str())
        .bind(input.measured_bytes)
        .bind(&input.final_path)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        inserted.try_into()
    }

    /// Return one run-dir row by identity.
    pub async fn get(&self, key: &RunDirKey) -> DbResult<Option<RunDirRow>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, RunDirDbRow>(&format!(
            "SELECT {RUN_DIR_COLUMNS} FROM run_dirs WHERE volume_id = $1 AND pod_uid = $2"
        ))
        .bind(&key.volume_id)
        .bind(&key.pod_uid)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// List every run-dir row on a volume, ordered by pod UID.
    pub async fn list_by_volume(&self, volume_id: &str) -> DbResult<Vec<RunDirRow>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, RunDirDbRow>(&format!(
            "SELECT {RUN_DIR_COLUMNS} FROM run_dirs WHERE volume_id = $1 ORDER BY pod_uid"
        ))
        .bind(volume_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Aggregate per-state count and byte totals for one volume.
    ///
    /// Only states with at least one row are returned; callers fill absent
    /// states with zero for bounded telemetry.
    pub async fn volume_state_totals(&self, volume_id: &str) -> DbResult<Vec<RunDirStateTotals>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, (String, i64, i64, i64)>(
            "SELECT state, COUNT(*)::bigint, \
                    COALESCE(SUM(reserved_bytes), 0)::bigint, \
                    COALESCE(SUM(measured_bytes), 0)::bigint \
             FROM run_dirs WHERE volume_id = $1 GROUP BY state ORDER BY state",
        )
        .bind(volume_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(|(state, count, reserved_bytes, measured_bytes)| {
                Ok(RunDirStateTotals {
                    state: RunDirState::parse(&state)?,
                    count,
                    reserved_bytes,
                    measured_bytes,
                })
            })
            .collect()
    }

    /// Return the most recent successful measured-byte reading for the seed
    /// projection of a `(project, base fingerprint)` pair.
    pub async fn latest_measured_bytes(
        &self,
        project_id: &str,
        base_fingerprint: &str,
    ) -> DbResult<Option<i64>> {
        self.db.ensure_initialized().await?;
        sqlx::query_scalar(
            "SELECT measured_bytes FROM run_dirs \
             WHERE project_id = $1 AND base_fingerprint = $2 \
               AND state IN ('ready_active', 'ready_idle') AND measured_bytes > 0 \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(project_id)
        .bind(base_fingerprint)
        .fetch_optional(self.db.pool())
        .await
        .map_err(Into::into)
    }
}

/// Field updates carried by a compare-and-set transition.
struct TransitionSpec<'a> {
    to: RunDirState,
    bump_generation: bool,
    temp_path: Option<&'a str>,
    final_path: Option<&'a str>,
    measured_bytes: Option<i64>,
    release_reservation: bool,
    touch_lease: bool,
}

impl TransitionSpec<'_> {
    fn default_for(to: RunDirState) -> Self {
        Self {
            to,
            bump_generation: false,
            temp_path: None,
            final_path: None,
            measured_bytes: None,
            release_reservation: false,
            touch_lease: false,
        }
    }
}

#[derive(sqlx::FromRow)]
struct RunDirDbRow {
    volume_id: String,
    pod_uid: String,
    task_run_id: Option<String>,
    project_id: Option<String>,
    base_fingerprint: Option<String>,
    state: String,
    generation: i64,
    reserved_bytes: i64,
    measured_bytes: i64,
    quota_id: Option<String>,
    last_lease_at: Option<String>,
    temp_path: Option<String>,
    final_path: Option<String>,
    created_at: String,
    updated_at: String,
}

impl TryFrom<RunDirDbRow> for RunDirRow {
    type Error = DbError;

    fn try_from(value: RunDirDbRow) -> Result<Self, Self::Error> {
        Ok(Self {
            key: RunDirKey {
                volume_id: value.volume_id,
                pod_uid: value.pod_uid,
            },
            task_run_id: value.task_run_id,
            project_id: value.project_id,
            base_fingerprint: value.base_fingerprint,
            state: RunDirState::parse(&value.state)?,
            generation: value.generation,
            reserved_bytes: value.reserved_bytes,
            measured_bytes: value.measured_bytes,
            quota_id: value.quota_id,
            last_lease_at: value.last_lease_at,
            temp_path: value.temp_path,
            final_path: value.final_path,
            created_at: value.created_at,
            updated_at: value.updated_at,
        })
    }
}

async fn lock_volume(tx: &mut Transaction<'_, Postgres>, volume_id: &str) -> DbResult<()> {
    let lock_key = format!("run-dir-volume:{volume_id}");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn fetch_for_update(
    tx: &mut Transaction<'_, Postgres>,
    key: &RunDirKey,
) -> DbResult<Option<RunDirRow>> {
    let row = sqlx::query_as::<_, RunDirDbRow>(&format!(
        "SELECT {RUN_DIR_COLUMNS} FROM run_dirs \
         WHERE volume_id = $1 AND pod_uid = $2 FOR UPDATE"
    ))
    .bind(&key.volume_id)
    .bind(&key.pod_uid)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}

#[cfg(test)]
#[path = "run_dirs_tests.rs"]
mod tests;
