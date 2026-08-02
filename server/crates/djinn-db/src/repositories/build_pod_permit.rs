//! Durable PostgreSQL build-pod permit admission and Pod resize identity.
//!
//! Capacity is deliberately serialized by the singleton `global` pool row from
//! migration 162. A missing pool/table or any database error is an unavailable
//! outcome, never an admission decision.
//!
//! Migration 164 extends the same relation — deliberately not a second lease
//! table — with the write-once native-sidecar resize identity and the
//! nonterminal resize lifecycle. Capacity accounting is unchanged: every
//! resize state is simply another `state <> 'released'` shape.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

const POOL_KEY: &str = "global";
// `state_age_seconds` is computed by the DATABASE, from the same `now()` the
// migration-170 trigger stamps `state_changed_at` with. Rendering the timestamp
// and subtracting it in Rust would compare two clocks — a server whose clock
// runs behind Postgres would read every row as young and grace a genuinely
// stranded drop forever, which is the failure this column was added to end.
// `GREATEST` clamps the one case a single clock can still produce: a row
// stamped by a transaction that committed after this SELECT's snapshot.
const ROW_COLUMNS: &str = "task_run_id, permit_id::text AS permit_id, fencing_token, state, job_uid, \
    to_char(acquired_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS acquired_at, \
    to_char(released_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS released_at, \
    released_fencing_token, release_reason, pod_namespace, pod_name, pod_uid, launcher_container_name, launcher_container_id, image_digest, observed_launcher_protocol, effective_launcher_protocol, admitted_cpu_millicores, resize_invocation_id, \
    GREATEST(0, EXTRACT(EPOCH FROM (now() - state_changed_at)))::bigint AS state_age_seconds";

/// Durable lifecycle states. `acquired`/`job_created`/`released` come from
/// migration 162; the six nonterminal resize states come from migration 164.
///
/// Only `released` is terminal for capacity: every resize state still counts
/// against the pool, exactly as `job_created` always has.
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

impl BuildPodPermitState {
    /// The durable spelling. This is the single place the Rust enum and the
    /// migration-164 `build_pod_permits_state_check` vocabulary are tied
    /// together, so a new variant cannot silently bind as an unchecked string.
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

/// The nonterminal resize/lease lifecycle states introduced by migration 164.
///
/// This is the single Rust-side definition of that set. It is deliberately a
/// `const` rather than six string literals repeated at each use site: the
/// authority-mode drain fence
/// ([`crate::repositories::launcher_authority_mode`]) partitions the active
/// permits on exactly this set, and a seventh resize state added to
/// [`BuildPodPermitState`] but forgotten in one hand-written `IN (...)` list
/// would be a Pod that is live, owed a drop, and invisible to the fence that
/// exists to refuse a flip while it is.
pub(crate) const NONTERMINAL_RESIZE_STATES: [BuildPodPermitState; 6] = [
    BuildPodPermitState::BirthConfirmed,
    BuildPodPermitState::LiftApplying,
    BuildPodPermitState::Lifted,
    BuildPodPermitState::DropRequired,
    BuildPodPermitState::DropApplying,
    BuildPodPermitState::Quarantined,
];

/// The two states a permit occupies *before* the resize birth capture.
///
/// Deliberately the complement of [`NONTERMINAL_RESIZE_STATES`] within the
/// unreleased set: together the two lists cover every state a live permit can be
/// in, so a row cannot fall between the reconciler's two scans and be reaped by
/// neither.
pub(crate) const PRE_BIRTH_STATES: [BuildPodPermitState; 2] = [
    BuildPodPermitState::Acquired,
    BuildPodPermitState::JobCreated,
];

/// Render states as a SQL literal list body, e.g. `'lifted', 'quarantined'`.
///
/// The spellings come from [`BuildPodPermitState::as_str`], which migration
/// 164's `build_pod_permits_state_check` already constrains, so a list built
/// this way cannot drift from the durable vocabulary.
pub(crate) fn sql_state_list(states: &[BuildPodPermitState]) -> String {
    states
        .iter()
        .map(|state| format!("'{}'", state.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Write-once native-sidecar resize identity for one Pod UID.
///
/// The whole struct is all-or-nothing in the database
/// (`build_pod_permits_resize_identity_check`), so a partially observed Pod can
/// never be persisted as a half-captured identity.
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
    /// Mirror the migration-164 CHECK constraints in Rust so a malformed
    /// identity is a typed `Rejected`, not a database error the caller would
    /// have to classify. The database remains the authority; this only avoids
    /// spending a round trip to be told the same thing.
    fn valid(&self) -> bool {
        !self.pod_namespace.trim().is_empty()
            && !self.pod_name.trim().is_empty()
            && !self.pod_uid.trim().is_empty()
            && !self.launcher_container_name.trim().is_empty()
            && !self.launcher_container_id.trim().is_empty()
            && !self.image_digest.trim().is_empty()
            && self
                .observed_launcher_protocol
                .parse::<djinn_launcher_protocol::LauncherAuthorityProtocol>()
                .is_ok()
            && self
                .effective_launcher_protocol
                .parse::<djinn_launcher_protocol::LauncherAuthorityProtocol>()
                .is_ok()
            && self.admitted_cpu_millicores > 0
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
    /// The invocation that currently owns this row's resize lifecycle.
    ///
    /// `None` means no invocation has ever lifted this Pod — the row is at
    /// `birth_confirmed` after capture, or it was driven straight to
    /// `drop_required`/`quarantined` by an infrastructure observer that has no
    /// invocation to name. See migration 168.
    pub resize_invocation_id: Option<String>,
    /// How long this row has rested in [`Self::state`], in whole seconds, as
    /// measured by the database at the instant of the read.
    ///
    /// It is an AGE and not a timestamp on purpose. The consumer —
    /// `task_run_resize_reconcile`'s strand predicate — asks exactly one
    /// question of it ("has this row been here longer than any live driver could
    /// hold it?"), and answering that from a rendered timestamp would mean
    /// subtracting the reader's wall clock from the writer's. See
    /// [`Self::state_age`] and migration 170.
    pub state_age_seconds: i64,
}

impl BuildPodPermitRow {
    /// [`Self::state_age_seconds`] as a [`Duration`].
    ///
    /// A negative age is impossible by construction (the SELECT clamps it) and
    /// is folded to zero here anyway, because the safe reading of "I cannot tell
    /// how old this is" is "it is brand new" — that grants a live driver the
    /// benefit of the doubt instead of taking a drop away from it.
    #[must_use]
    pub fn state_age(&self) -> Duration {
        Duration::from_secs(u64::try_from(self.state_age_seconds).unwrap_or(0))
    }
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

/// The result of the write-once resize identity capture. Like the bind result
/// above, an exact replay is idempotent and anything else is `Rejected`
/// *without* mutating the row, which is the caller's cue to fail closed and
/// delete the Pod.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureBuildPodResizeIdentityResult {
    Captured(BuildPodPermitRow),
    AlreadyCaptured(BuildPodPermitRow),
    Rejected,
}

/// The result of one compare-and-swap resize lifecycle transition.
///
/// The row is boxed here and not in the sibling result enums above, and that
/// asymmetry is deliberate rather than lint appeasement. Those enums each carry
/// a `BuildPodPermitRow` in *two* variants, so boxing would not shrink them —
/// they are row-sized either way. This enum carries a row in exactly one
/// variant next to a unit `Rejected`, so inlining the row would make every
/// rejection pay for a ~376-byte return value (`clippy::large_enum_variant`).
/// Boxing keeps it pointer-sized no matter how the row grows later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionBuildPodResizeLifecycleResult {
    Transitioned(Box<BuildPodPermitRow>),
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

    /// Age a row's `state_changed_at` backwards so a test can reach the far
    /// side of a grace window without spending it in wall clock.
    ///
    /// Test-support-only, and deliberately so: the migration-170 trigger stamps
    /// `state_changed_at` on every state change and no production write path
    /// names the column at all, which is what makes the age unforgeable by a
    /// caller. This seam does not weaken that — it changes no state, so the
    /// stamp it moves is one the trigger will overwrite the moment the row's
    /// lifecycle advances again.
    ///
    /// # Errors
    ///
    /// Propagates any durable failure.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn backdate_state_change_for_test(
        &self,
        task_run_id: &str,
        age: Duration,
    ) -> DbResult<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE build_pod_permits \
             SET state_changed_at = now() - make_interval(secs => $1) \
             WHERE task_run_id = $2",
        )
        .bind(age.as_secs_f64())
        .bind(task_run_id)
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
                // `Unavailable` is deliberately undifferentiated so this API
                // fails closed, which means this log line is the *only* place
                // the actual cause is ever named. It carries `task_run_id`
                // because without it a foreign-key violation here is
                // indistinguishable from a pool outage at the other end of the
                // dispatch log.
                tracing::warn!(
                    task_run_id,
                    error = %error,
                    "build pod permit acquisition unavailable"
                );
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

    /// Capture the write-once resize identity and enter `birth_confirmed`.
    ///
    /// The identity and the state change are one conditional statement, so a
    /// permit can never sit in a resize state without the identity that fences
    /// it. `pod_uid IS NULL` is the write-once predicate: once any identity is
    /// durable, no second capture can update the row, and the migration-164
    /// trigger independently rejects the mutation even if this predicate were
    /// ever loosened. A losing caller therefore observes `Rejected` with the
    /// first writer's row intact.
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
        let updated = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_pod_permits SET state = 'birth_confirmed', \
             pod_namespace = $1, pod_name = $2, pod_uid = $3, \
             launcher_container_name = $4, launcher_container_id = $5, \
             image_digest = $6, observed_launcher_protocol = $7, \
             effective_launcher_protocol = $8, admitted_cpu_millicores = $9 \
             WHERE task_run_id = $10 AND permit_id = $11::uuid AND fencing_token = $12 \
               AND state = 'job_created' AND pod_uid IS NULL RETURNING {ROW_COLUMNS}"
        ))
        .bind(&identity.pod_namespace)
        .bind(&identity.pod_name)
        .bind(&identity.pod_uid)
        .bind(&identity.launcher_container_name)
        .bind(&identity.launcher_container_id)
        .bind(&identity.image_digest)
        .bind(&identity.observed_launcher_protocol)
        .bind(&identity.effective_launcher_protocol)
        .bind(identity.admitted_cpu_millicores)
        .bind(task_run_id)
        .bind(permit_id)
        .bind(fencing_token)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = updated {
            tx.commit().await?;
            return Ok(CaptureBuildPodResizeIdentityResult::Captured(
                row.try_into()?,
            ));
        }
        let existing = fetch_tx(&mut tx, task_run_id).await?;
        tx.commit().await?;
        Ok(match existing {
            // Only a byte-identical replay under the same permit and fence is
            // idempotent. Any differing field — Pod UID, coordinates, launcher
            // identity, digest, protocol or ceiling — falls through to
            // `Rejected` because the whole identity is compared, not just the
            // Pod UID that the write-once predicate keys on.
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

    /// Claim the resize lifecycle for one invocation: `birth_confirmed →
    /// lift_applying`, writing `resize_invocation_id`.
    ///
    /// This is the ONLY edge on which migration 168's trigger permits
    /// `resize_invocation_id` to change, so it is the only way an invocation can
    /// become the row's owner. Two invocations of one task run may be in flight
    /// simultaneously — nothing in `djinn_agent::process`'s `'invocation: loop`
    /// serializes them, the runner is `Arc`-shared behind `&self`, and its
    /// journal keeps a `HashSet` of live invocation ids precisely because more
    /// than one can be live. The single-row lifecycle cannot represent two
    /// simultaneous lifts, so the second claimant must lose, and this CAS is
    /// where it loses: `state = 'birth_confirmed'` holds for exactly one of them.
    ///
    /// An exact replay by the invocation that already owns the row is
    /// idempotent; anything else is `Rejected` **without a write**, which is
    /// what lets a caller assert a PATCH counter of zero.
    pub async fn begin_resize_invocation(
        &self,
        task_run_id: &str,
        permit_id: &str,
        fencing_token: i64,
        pod_uid: &str,
        invocation_id: &str,
    ) -> DbResult<TransitionBuildPodResizeLifecycleResult> {
        if pod_uid.trim().is_empty() || invocation_id.trim().is_empty() {
            return Ok(TransitionBuildPodResizeLifecycleResult::Rejected);
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let updated = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_pod_permits SET state = 'lift_applying', resize_invocation_id = $1 \
             WHERE task_run_id = $2 AND permit_id = $3::uuid AND fencing_token = $4 \
               AND pod_uid = $5 AND state = 'birth_confirmed' RETURNING {ROW_COLUMNS}"
        ))
        .bind(invocation_id)
        .bind(task_run_id)
        .bind(permit_id)
        .bind(fencing_token)
        .bind(pod_uid)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = updated {
            tx.commit().await?;
            return Ok(TransitionBuildPodResizeLifecycleResult::Transitioned(
                Box::new(row.try_into()?),
            ));
        }
        let existing = fetch_tx(&mut tx, task_run_id).await?;
        tx.commit().await?;
        Ok(match existing {
            Some(row)
                if row.permit_id == permit_id
                    && row.fencing_token == fencing_token
                    && row.state == BuildPodPermitState::LiftApplying
                    && row.resize_identity.as_ref().map(|id| id.pod_uid.as_str())
                        == Some(pod_uid)
                    && row.resize_invocation_id.as_deref() == Some(invocation_id) =>
            {
                TransitionBuildPodResizeLifecycleResult::Transitioned(Box::new(row))
            }
            _ => TransitionBuildPodResizeLifecycleResult::Rejected,
        })
    }

    /// Compare-and-swap one resize lifecycle transition.
    ///
    /// The predicate fences the task run, permit identity, fencing token, the
    /// captured Pod UID, **the owning invocation** and the expected current
    /// state in a single statement, so a stale observer cannot advance a
    /// lifecycle it no longer owns. The migration-164 trigger separately rejects
    /// transitions that are not on the legal edge list, which keeps the state
    /// machine authoritative in the database rather than in whichever caller
    /// happens to run.
    ///
    /// `invocation` is `None` for the infrastructure-observer paths that reach
    /// `drop_required`/`quarantined` from `birth_confirmed` without any
    /// invocation having lifted. It is compared with `IS NOT DISTINCT FROM`, so
    /// `None` means "the row must carry no invocation" rather than "do not
    /// check" — dropping this clause is acceptance criterion 1's named mutation.
    // Every parameter is one component of a single compare-and-swap predicate.
    // Bundling them into a struct would let a caller construct a HALF-SPECIFIED
    // fence and still compile, which is exactly the failure this method exists
    // to make impossible.
    #[allow(clippy::too_many_arguments)]
    pub async fn transition_resize_lifecycle(
        &self,
        task_run_id: &str,
        permit_id: &str,
        fencing_token: i64,
        pod_uid: &str,
        invocation: Option<&str>,
        expected: BuildPodPermitState,
        next: BuildPodPermitState,
    ) -> DbResult<TransitionBuildPodResizeLifecycleResult> {
        if pod_uid.trim().is_empty() {
            return Ok(TransitionBuildPodResizeLifecycleResult::Rejected);
        }
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, DbRow>(&format!(
            "UPDATE build_pod_permits SET state = $1 \
             WHERE task_run_id = $2 AND permit_id = $3::uuid AND fencing_token = $4 \
               AND pod_uid = $5 AND resize_invocation_id IS NOT DISTINCT FROM $6 \
               AND state = $7 RETURNING {ROW_COLUMNS}"
        ))
        .bind(next.as_str())
        .bind(task_run_id)
        .bind(permit_id)
        .bind(fencing_token)
        .bind(pod_uid)
        .bind(invocation)
        .bind(expected.as_str())
        .fetch_optional(self.db.pool())
        .await?;
        Ok(match row {
            Some(row) => {
                TransitionBuildPodResizeLifecycleResult::Transitioned(Box::new(row.try_into()?))
            }
            None => TransitionBuildPodResizeLifecycleResult::Rejected,
        })
    }

    /// List every permit sitting in a nonterminal resize state.
    ///
    /// This is the restart-readable recovery view: after a controller restart
    /// the durable row, not in-process memory, says which Pods still owe a lift,
    /// a drop or a quarantine decision. The ordering matches the
    /// `build_pod_permits_resize_nonterminal_idx` partial index from migration
    /// 164.
    pub async fn list_nonterminal_resize(&self) -> DbResult<Vec<BuildPodPermitRow>> {
        self.db.ensure_initialized().await?;
        let nonterminal = sql_state_list(&NONTERMINAL_RESIZE_STATES);
        sqlx::query_as::<_, DbRow>(&format!(
            "SELECT {ROW_COLUMNS} FROM build_pod_permits \
             WHERE state IN ({nonterminal}) \
             ORDER BY acquired_at, task_run_id"
        ))
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }

    /// List permits that never reached the resize lifecycle and have not moved
    /// for `older_than`.
    ///
    /// # The gap this closes
    ///
    /// [`Self::list_nonterminal_resize`] scans the six states from migration 164
    /// — every one of which is *past* the birth capture. A permit that is
    /// refused, or whose dispatch dies, before `capture_resize_identity` ever
    /// succeeds never enters that set: it rests at `acquired` or `job_created`
    /// with `pod_uid IS NULL` and is invisible to the reconciler forever. On
    /// 2026-08-02 production held 70 such rows, the oldest ~10 hours, against
    /// three live task-run Jobs, and nothing in the system could see them.
    ///
    /// `pod_uid IS NULL` is part of the predicate rather than an assumption: it
    /// is what makes a returned row *provably* pre-capture, so a caller can
    /// retire it without racing the resize lifecycle for a Pod it never owned.
    ///
    /// Ordered oldest-first so a bounded pass drains the worst backlog first.
    pub async fn list_pre_birth_stranded(
        &self,
        older_than: Duration,
    ) -> DbResult<Vec<BuildPodPermitRow>> {
        self.db.ensure_initialized().await?;
        let pre_birth = sql_state_list(&PRE_BIRTH_STATES);
        sqlx::query_as::<_, DbRow>(&format!(
            "SELECT {ROW_COLUMNS} FROM build_pod_permits \
             WHERE state IN ({pre_birth}) \
               AND pod_uid IS NULL \
               AND state_changed_at <= now() - make_interval(secs => $1) \
             ORDER BY state_changed_at, task_run_id"
        ))
        .bind(older_than.as_secs_f64())
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
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
    ///
    /// The predicate is `state <> 'released'` rather than an enumeration of
    /// releasable states. Before migration 164 those were the same set, so this
    /// is a no-op for every row production can currently hold; afterwards it is
    /// what keeps a permit stuck in a resize state from becoming unreleasable
    /// and leaking a pool unit forever. Enumerating states here would mean the
    /// release path has to be edited in lockstep with every future lifecycle
    /// state, and forgetting once wedges admission cluster-wide.
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
    resize_invocation_id: Option<String>,
    state_age_seconds: i64,
}

impl TryFrom<DbRow> for BuildPodPermitRow {
    type Error = DbError;

    fn try_from(row: DbRow) -> DbResult<Self> {
        let resize_invocation_id = row.resize_invocation_id.clone();
        Ok(Self {
            resize_invocation_id,
            state_age_seconds: row.state_age_seconds,
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
