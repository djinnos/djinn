//! Durable, fenced admission-authority handoff state.
//!
//! The handoff row is deliberately separate from the workload admission journal:
//! it records which authority may admit work, not the work that has been admitted.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

const HANDOFF_NAME: &str = "build";
const HANDOFF_COLUMNS: &str = "name, phase, epoch, emergency_ack_epoch, invocation_ack_epoch, \
     v0_mode, v1_mode, cap, updated_at::text";

/// The only supported admission-authority phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionHandoffPhase {
    EmergencyPrimary,
    ForwardOverlap,
    InvocationPrimary,
    RollbackOverlap,
}

impl AdmissionHandoffPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::EmergencyPrimary => "emergency_primary",
            Self::ForwardOverlap => "forward_overlap",
            Self::InvocationPrimary => "invocation_primary",
            Self::RollbackOverlap => "rollback_overlap",
        }
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "emergency_primary" => Ok(Self::EmergencyPrimary),
            "forward_overlap" => Ok(Self::ForwardOverlap),
            "invocation_primary" => Ok(Self::InvocationPrimary),
            "rollback_overlap" => Ok(Self::RollbackOverlap),
            _ => Err(DbError::InvalidData(format!(
                "invalid admission handoff phase `{value}`"
            ))),
        }
    }

    fn legal_next(self) -> Self {
        match self {
            Self::EmergencyPrimary => Self::ForwardOverlap,
            Self::ForwardOverlap => Self::InvocationPrimary,
            Self::InvocationPrimary => Self::RollbackOverlap,
            Self::RollbackOverlap => Self::EmergencyPrimary,
        }
    }
}

/// Authority that has confirmed it is healthy for a handoff epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionHandoffAuthority {
    Emergency,
    Invocation,
}

/// Mode of the v0 (emergency) admission authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V0Mode {
    /// Atomically enforce the reference cap.
    Enforce,
    /// Record reservations but never deny.
    Observe,
    /// The v0 authority is released.
    Disabled,
}

impl V0Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Observe => "observe",
            Self::Disabled => "disabled",
        }
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "enforce" => Ok(Self::Enforce),
            "observe" => Ok(Self::Observe),
            "disabled" => Ok(Self::Disabled),
            _ => Err(DbError::InvalidData(format!(
                "invalid v0 admission mode `{value}`"
            ))),
        }
    }

    /// Only `Enforce` actually denies over the cap; observe/disabled do not.
    #[must_use]
    pub const fn is_enforcing(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// Mode of the v1 (invocation) admission authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum V1Mode {
    /// The v1 authority is not running.
    Off,
    /// The v1 authority observes but never denies.
    Shadow,
    /// The v1 authority atomically enforces the reference cap.
    Enforce,
}

impl V1Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "enforce" => Ok(Self::Enforce),
            _ => Err(DbError::InvalidData(format!(
                "invalid v1 admission mode `{value}`"
            ))),
        }
    }

    /// Only `Enforce` actually denies over the cap; off/shadow do not.
    #[must_use]
    pub const fn is_enforcing(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// The single durable build-admission handoff record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionHandoffRow {
    pub phase: AdmissionHandoffPhase,
    pub epoch: i64,
    pub emergency_ack_epoch: Option<i64>,
    pub invocation_ack_epoch: Option<i64>,
    /// Mode of the v0 (emergency) authority for this epoch.
    pub v0_mode: V0Mode,
    /// Mode of the v1 (invocation) authority for this epoch.
    pub v1_mode: V1Mode,
    /// Reference concurrency cap; `None` defers to the controller default.
    pub cap: Option<i64>,
    /// RFC3339-formatted by PostgreSQL.
    pub updated_at: String,
}

/// Transactional Postgres repository for the `build` handoff singleton.
pub struct AdmissionHandoffRepository {
    db: Database,
}

impl AdmissionHandoffRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Read the singleton without taking a write lock. `None` is retained as a
    /// meaningful result for installations that have not applied this migration.
    pub async fn read(&self) -> DbResult<Option<AdmissionHandoffRow>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, HandoffDbRow>(&format!(
            "SELECT {HANDOFF_COLUMNS} FROM admission_handoff WHERE name = $1"
        ))
        .bind(HANDOFF_NAME)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Remove the singleton to exercise startup behavior for installations
    /// where no durable handoff has been created.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn delete_for_test(&self) -> DbResult<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM admission_handoff WHERE name = $1")
            .bind(HANDOFF_NAME)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Record one authority's acknowledgement only when `epoch` is still current.
    /// Repeating the same current acknowledgement is idempotent.
    pub async fn acknowledge(
        &self,
        authority: AdmissionHandoffAuthority,
        epoch: i64,
    ) -> DbResult<AdmissionHandoffRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx).await?;
        if row.epoch != epoch {
            return Err(DbError::InvalidTransition(format!(
                "stale admission handoff acknowledgement epoch {epoch}; current epoch is {}",
                row.epoch
            )));
        }
        let column = match authority {
            AdmissionHandoffAuthority::Emergency => "emergency_ack_epoch",
            AdmissionHandoffAuthority::Invocation => "invocation_ack_epoch",
        };
        let updated = sqlx::query_as::<_, HandoffDbRow>(&format!(
            "UPDATE admission_handoff SET {column} = $1, updated_at = now() \
             WHERE name = $2 RETURNING {HANDOFF_COLUMNS}"
        ))
        .bind(epoch)
        .bind(HANDOFF_NAME)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        updated.try_into()
    }

    /// Compare-and-swap the v0/v1 modes and reference cap at the current epoch.
    ///
    /// Serialized inside the same row lock as [`Self::advance`] and
    /// [`Self::acknowledge`], so a mode/cap change and a phase advance cannot
    /// both apply from the same stale epoch: applying a change increments the
    /// epoch, clears the per-authority acknowledgements, and retires the
    /// per-generation acknowledgements of superseded epochs.
    pub async fn set_modes_and_cap(
        &self,
        expected_epoch: i64,
        v0: V0Mode,
        v1: V1Mode,
        cap: Option<i64>,
    ) -> DbResult<AdmissionHandoffRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx).await?;
        if row.epoch != expected_epoch {
            return Err(DbError::InvalidTransition(format!(
                "stale admission handoff expected epoch {expected_epoch}; current epoch is {}",
                row.epoch
            )));
        }
        let updated = sqlx::query_as::<_, HandoffDbRow>(&format!(
            "UPDATE admission_handoff \
             SET v0_mode = $1, v1_mode = $2, cap = $3, epoch = epoch + 1, \
                 emergency_ack_epoch = NULL, invocation_ack_epoch = NULL, updated_at = now() \
             WHERE name = $4 AND epoch = $5 RETURNING {HANDOFF_COLUMNS}"
        ))
        .bind(v0.as_str())
        .bind(v1.as_str())
        .bind(cap)
        .bind(HANDOFF_NAME)
        .bind(expected_epoch)
        .fetch_one(&mut *tx)
        .await?;
        retire_stale_generation_acks(&mut tx, updated.epoch).await?;
        tx.commit().await?;
        updated.try_into()
    }

    /// Record one live generation's current-epoch acknowledgement.
    ///
    /// The `generation_key` follows the [`crate::AdmissionJournalKey`]
    /// `{domain, work_id, generation}` identity convention. Recording is
    /// idempotent (repeats of the same current key are accepted) and
    /// stale-fenced: an acknowledgement for a superseded epoch is rejected so
    /// the generation re-acknowledges the current epoch.
    pub async fn record_generation_ack(&self, epoch: i64, generation_key: &str) -> DbResult<()> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx).await?;
        if row.epoch != epoch {
            return Err(DbError::InvalidTransition(format!(
                "stale admission handoff generation acknowledgement epoch {epoch}; \
                 current epoch is {}",
                row.epoch
            )));
        }
        sqlx::query(
            "INSERT INTO admission_handoff_generation_ack (epoch, generation_key) \
             VALUES ($1, $2) ON CONFLICT (epoch, generation_key) DO NOTHING",
        )
        .bind(epoch)
        .bind(generation_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Whether every generation in `required` has a current-epoch ack row.
    ///
    /// An empty required set is trivially complete: an epoch with no armed live
    /// generation has nothing to wait for.
    pub async fn generation_ack_complete(&self, epoch: i64, required: &[String]) -> DbResult<bool> {
        if required.is_empty() {
            return Ok(true);
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let acked = generation_acks_at_epoch(&mut tx, epoch).await?;
        tx.commit().await?;
        Ok(required.iter().all(|key| acked.contains(key)))
    }

    /// Compare-and-swap from the current phase to its only legal successor.
    ///
    /// Each edge requires evidence at the current epoch. Entering an overlap
    /// requires the outgoing primary authority; leaving an overlap requires both
    /// authorities, so neither authority can be disabled without confirming that
    /// the other remains healthy for this exact epoch.
    ///
    /// The InvocationPrimary edge (leaving the forward overlap) additionally
    /// requires that every generation in `required_generations` — the live set
    /// captured when the overlap was armed — has an idempotent current-epoch
    /// acknowledgement, so v0 is released only once every live generation has
    /// confirmed v1. Every other edge ignores `required_generations`.
    pub async fn advance(
        &self,
        expected_epoch: i64,
        next_phase: AdmissionHandoffPhase,
        required_generations: &[String],
    ) -> DbResult<AdmissionHandoffRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx).await?;
        if row.epoch != expected_epoch {
            return Err(DbError::InvalidTransition(format!(
                "stale admission handoff expected epoch {expected_epoch}; current epoch is {}",
                row.epoch
            )));
        }
        if row.phase.legal_next() != next_phase {
            return Err(DbError::InvalidTransition(format!(
                "illegal admission handoff phase advance from {:?} to {:?}",
                row.phase, next_phase
            )));
        }
        require_acknowledgements(&row)?;
        if next_phase == AdmissionHandoffPhase::InvocationPrimary {
            let acked = generation_acks_at_epoch(&mut tx, expected_epoch).await?;
            if let Some(missing) = required_generations
                .iter()
                .find(|key| !acked.contains(*key))
            {
                return Err(DbError::InvalidTransition(format!(
                    "admission handoff invocation-primary edge blocked at epoch {expected_epoch}: \
                     generation `{missing}` has not acknowledged"
                )));
            }
        }
        let updated = sqlx::query_as::<_, HandoffDbRow>(&format!(
            "UPDATE admission_handoff \
             SET phase = $1, epoch = epoch + 1, emergency_ack_epoch = NULL, \
                 invocation_ack_epoch = NULL, updated_at = now() \
             WHERE name = $2 AND epoch = $3 RETURNING {HANDOFF_COLUMNS}"
        ))
        .bind(next_phase.as_str())
        .bind(HANDOFF_NAME)
        .bind(expected_epoch)
        .fetch_one(&mut *tx)
        .await?;
        retire_stale_generation_acks(&mut tx, updated.epoch).await?;
        tx.commit().await?;
        updated.try_into()
    }
}

#[derive(sqlx::FromRow)]
struct HandoffDbRow {
    name: String,
    phase: String,
    epoch: i64,
    emergency_ack_epoch: Option<i64>,
    invocation_ack_epoch: Option<i64>,
    v0_mode: String,
    v1_mode: String,
    cap: Option<i64>,
    updated_at: String,
}

impl TryFrom<HandoffDbRow> for AdmissionHandoffRow {
    type Error = DbError;

    fn try_from(value: HandoffDbRow) -> Result<Self, Self::Error> {
        if value.name != HANDOFF_NAME {
            return Err(DbError::InvalidData(format!(
                "invalid admission handoff singleton `{}`",
                value.name
            )));
        }
        if value.epoch < 0 {
            return Err(DbError::InvalidData(
                "negative admission handoff epoch".into(),
            ));
        }
        Ok(Self {
            phase: AdmissionHandoffPhase::parse(&value.phase)?,
            epoch: value.epoch,
            emergency_ack_epoch: value.emergency_ack_epoch,
            invocation_ack_epoch: value.invocation_ack_epoch,
            v0_mode: V0Mode::parse(&value.v0_mode)?,
            v1_mode: V1Mode::parse(&value.v1_mode)?,
            cap: value.cap,
            updated_at: value.updated_at,
        })
    }
}

/// Read every generation key acknowledged at `epoch` under the held row lock.
async fn generation_acks_at_epoch(
    tx: &mut Transaction<'_, Postgres>,
    epoch: i64,
) -> DbResult<HashSet<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT generation_key FROM admission_handoff_generation_ack WHERE epoch = $1",
    )
    .bind(epoch)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(|(key,)| key).collect())
}

/// Drop per-generation acknowledgements for every epoch before `current_epoch`.
///
/// Called inside the same transaction that advanced the epoch, so the table
/// only ever holds acknowledgements for the live epoch.
async fn retire_stale_generation_acks(
    tx: &mut Transaction<'_, Postgres>,
    current_epoch: i64,
) -> DbResult<()> {
    sqlx::query("DELETE FROM admission_handoff_generation_ack WHERE epoch < $1")
        .bind(current_epoch)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn current_row_for_update(
    tx: &mut Transaction<'_, Postgres>,
) -> DbResult<AdmissionHandoffRow> {
    let row = sqlx::query_as::<_, HandoffDbRow>(&format!(
        "SELECT {HANDOFF_COLUMNS} FROM admission_handoff WHERE name = $1 FOR UPDATE"
    ))
    .bind(HANDOFF_NAME)
    .fetch_optional(&mut **tx)
    .await?;
    row.ok_or_else(|| DbError::InvalidTransition("admission handoff singleton is absent".into()))?
        .try_into()
}

fn require_acknowledgements(row: &AdmissionHandoffRow) -> DbResult<()> {
    let emergency_current = row.emergency_ack_epoch == Some(row.epoch);
    let invocation_current = row.invocation_ack_epoch == Some(row.epoch);
    let valid = match row.phase {
        AdmissionHandoffPhase::EmergencyPrimary => emergency_current,
        AdmissionHandoffPhase::ForwardOverlap => emergency_current && invocation_current,
        AdmissionHandoffPhase::InvocationPrimary => invocation_current,
        AdmissionHandoffPhase::RollbackOverlap => emergency_current && invocation_current,
    };
    if valid {
        Ok(())
    } else {
        Err(DbError::InvalidTransition(format!(
            "missing current-epoch acknowledgement for {:?} at epoch {}",
            row.phase, row.epoch
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repository() -> AdmissionHandoffRepository {
        AdmissionHandoffRepository::new(Database::open_in_memory().unwrap())
    }

    async fn move_to(repo: &AdmissionHandoffRepository, phase: AdmissionHandoffPhase) {
        loop {
            let row = repo.read().await.unwrap().unwrap();
            if row.phase == phase {
                return;
            }
            match row.phase {
                AdmissionHandoffPhase::EmergencyPrimary => {
                    repo.acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
                        .await
                        .unwrap();
                }
                AdmissionHandoffPhase::ForwardOverlap | AdmissionHandoffPhase::RollbackOverlap => {
                    repo.acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
                        .await
                        .unwrap();
                    repo.acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
                        .await
                        .unwrap();
                }
                AdmissionHandoffPhase::InvocationPrimary => {
                    repo.acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
                        .await
                        .unwrap();
                }
            }
            repo.advance(row.epoch, row.phase.legal_next(), &[])
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn seeded_build_singleton_has_exact_initial_state() {
        let repo = repository().await;
        let row = repo.read().await.unwrap().unwrap();
        assert_eq!(row.phase, AdmissionHandoffPhase::EmergencyPrimary);
        assert_eq!(row.epoch, 0);
        assert_eq!(row.emergency_ack_epoch, None);
        assert_eq!(row.invocation_ack_epoch, None);
    }

    #[tokio::test]
    async fn every_phase_advances_only_to_its_legal_next_edge() {
        let cases = [
            (
                AdmissionHandoffPhase::EmergencyPrimary,
                AdmissionHandoffPhase::ForwardOverlap,
            ),
            (
                AdmissionHandoffPhase::ForwardOverlap,
                AdmissionHandoffPhase::InvocationPrimary,
            ),
            (
                AdmissionHandoffPhase::InvocationPrimary,
                AdmissionHandoffPhase::RollbackOverlap,
            ),
            (
                AdmissionHandoffPhase::RollbackOverlap,
                AdmissionHandoffPhase::EmergencyPrimary,
            ),
        ];
        for (phase, next) in cases {
            let repo = repository().await;
            move_to(&repo, phase).await;
            let row = repo.read().await.unwrap().unwrap();
            match phase {
                AdmissionHandoffPhase::EmergencyPrimary => {
                    repo.acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
                        .await
                        .unwrap();
                }
                AdmissionHandoffPhase::ForwardOverlap | AdmissionHandoffPhase::RollbackOverlap => {
                    repo.acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
                        .await
                        .unwrap();
                    repo.acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
                        .await
                        .unwrap();
                }
                AdmissionHandoffPhase::InvocationPrimary => {
                    repo.acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
                        .await
                        .unwrap();
                }
            }
            let advanced = repo.advance(row.epoch, next, &[]).await.unwrap();
            assert_eq!(advanced.phase, next);
            assert_eq!(advanced.epoch, row.epoch + 1);
            assert_eq!(advanced.emergency_ack_epoch, None);
            assert_eq!(advanced.invocation_ack_epoch, None);
        }
    }

    #[tokio::test]
    async fn every_phase_rejects_missing_required_acknowledgements() {
        for phase in [
            AdmissionHandoffPhase::EmergencyPrimary,
            AdmissionHandoffPhase::ForwardOverlap,
            AdmissionHandoffPhase::InvocationPrimary,
            AdmissionHandoffPhase::RollbackOverlap,
        ] {
            let repo = repository().await;
            move_to(&repo, phase).await;
            let row = repo.read().await.unwrap().unwrap();
            assert!(matches!(
                repo.advance(row.epoch, row.phase.legal_next(), &[]).await,
                Err(DbError::InvalidTransition(_))
            ));
        }
    }

    #[tokio::test]
    async fn stale_epochs_acknowledgements_and_illegal_edges_are_rejected() {
        let repo = repository().await;
        let row = repo.read().await.unwrap().unwrap();
        assert!(matches!(
            repo.acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch + 1)
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        repo.acknowledge(AdmissionHandoffAuthority::Emergency, row.epoch)
            .await
            .unwrap();
        assert!(matches!(
            repo.advance(row.epoch, AdmissionHandoffPhase::InvocationPrimary, &[])
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        let advanced = repo
            .advance(row.epoch, AdmissionHandoffPhase::ForwardOverlap, &[])
            .await
            .unwrap();
        assert!(matches!(
            repo.advance(row.epoch, AdmissionHandoffPhase::InvocationPrimary, &[])
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        assert!(matches!(
            repo.acknowledge(AdmissionHandoffAuthority::Invocation, row.epoch)
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        assert_eq!(advanced.epoch, row.epoch + 1);
    }

    #[tokio::test]
    async fn set_modes_and_cap_round_trips_and_bumps_the_epoch() {
        let repo = repository().await;
        let seeded = repo.read().await.unwrap().unwrap();
        assert_eq!(seeded.v0_mode, V0Mode::Enforce);
        assert_eq!(seeded.v1_mode, V1Mode::Off);
        assert_eq!(seeded.cap, None);
        assert_eq!(seeded.epoch, 0);

        let updated = repo
            .set_modes_and_cap(0, V0Mode::Enforce, V1Mode::Shadow, Some(7))
            .await
            .unwrap();
        assert_eq!(updated.v0_mode, V0Mode::Enforce);
        assert_eq!(updated.v1_mode, V1Mode::Shadow);
        assert_eq!(updated.cap, Some(7));
        assert_eq!(updated.epoch, 1);
        assert_eq!(updated.emergency_ack_epoch, None);
        assert_eq!(updated.invocation_ack_epoch, None);

        // A stale-epoch mode change is rejected.
        assert!(matches!(
            repo.set_modes_and_cap(0, V0Mode::Observe, V1Mode::Enforce, None)
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        // The cap CHECK constraint rejects a non-positive cap.
        assert!(
            repo.set_modes_and_cap(1, V0Mode::Enforce, V1Mode::Off, Some(0))
                .await
                .is_err()
        );
        // The rejected writes left the durable row untouched.
        let current = repo.read().await.unwrap().unwrap();
        assert_eq!(current.epoch, 1);
        assert_eq!(current.v1_mode, V1Mode::Shadow);
        assert_eq!(current.cap, Some(7));
    }

    /// Acceptance criterion 1: a mode/cap change and a phase advance are both
    /// epoch-fenced compare-and-swaps serialized on the same row lock. With a
    /// paused transaction holding that lock, both mutations queue; when it is
    /// released exactly one applies (advancing the epoch) and the other is
    /// rejected as stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mode_change_and_phase_advance_cannot_both_apply_from_a_stale_epoch() {
        let db = Database::open_in_memory().unwrap();
        let repo = std::sync::Arc::new(AdmissionHandoffRepository::new(db.clone()));
        // Force the template clone / schema initialization before the manual lock.
        assert_eq!(repo.read().await.unwrap().unwrap().epoch, 0);
        // Emergency acknowledges epoch 0 so the ForwardOverlap advance is
        // otherwise legal and the only remaining fence is the epoch.
        repo.acknowledge(AdmissionHandoffAuthority::Emergency, 0)
            .await
            .unwrap();

        // A paused transaction holds the singleton row lock.
        let mut hold = db.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM admission_handoff WHERE name = 'build' FOR UPDATE")
            .fetch_one(&mut *hold)
            .await
            .unwrap();

        let advance = {
            let repo = std::sync::Arc::clone(&repo);
            tokio::spawn(async move {
                repo.advance(0, AdmissionHandoffPhase::ForwardOverlap, &[])
                    .await
            })
        };
        let modes = {
            let repo = std::sync::Arc::clone(&repo);
            tokio::spawn(async move {
                repo.set_modes_and_cap(0, V0Mode::Enforce, V1Mode::Shadow, Some(4))
                    .await
            })
        };
        // Let both spawned mutations block on the held FOR UPDATE lock.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // Release the paused transaction; the two mutations now serialize.
        hold.rollback().await.unwrap();

        let advance = advance.await.unwrap();
        let modes = modes.await.unwrap();
        let successes = [advance.is_ok(), modes.is_ok()]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(successes, 1, "exactly one epoch-0 mutation applies");
        let stale = [advance.err(), modes.err()].into_iter().flatten().count();
        assert_eq!(stale, 1, "the loser is rejected as a stale epoch");

        let current = repo.read().await.unwrap().unwrap();
        assert_eq!(
            current.epoch, 1,
            "the winning mutation advanced the epoch exactly once"
        );
    }

    /// Acceptance criterion 2: the InvocationPrimary edge stays closed until
    /// every generation in the armed live set has an idempotent current-epoch
    /// acknowledgement, then commits exactly once.
    #[tokio::test]
    async fn invocation_primary_edge_requires_every_armed_generation_to_acknowledge() {
        let repo = repository().await;
        // EmergencyPrimary(0) --emergency ack--> ForwardOverlap(1).
        repo.acknowledge(AdmissionHandoffAuthority::Emergency, 0)
            .await
            .unwrap();
        let overlap = repo
            .advance(0, AdmissionHandoffPhase::ForwardOverlap, &[])
            .await
            .unwrap();
        assert_eq!(overlap.phase, AdmissionHandoffPhase::ForwardOverlap);
        assert_eq!(overlap.epoch, 1);

        // Leaving the overlap needs both authority acks first.
        repo.acknowledge(AdmissionHandoffAuthority::Emergency, 1)
            .await
            .unwrap();
        repo.acknowledge(AdmissionHandoffAuthority::Invocation, 1)
            .await
            .unwrap();
        let armed = vec![
            "warm_build/alpha/0".to_string(),
            "task_observation/beta/3".to_string(),
        ];

        // No generation has acknowledged: the edge is blocked.
        assert!(!repo.generation_ack_complete(1, &armed).await.unwrap());
        assert!(matches!(
            repo.advance(1, AdmissionHandoffPhase::InvocationPrimary, &armed)
                .await,
            Err(DbError::InvalidTransition(_))
        ));

        // One generation acknowledges: still blocked.
        repo.record_generation_ack(1, &armed[0]).await.unwrap();
        assert!(!repo.generation_ack_complete(1, &armed).await.unwrap());
        assert!(matches!(
            repo.advance(1, AdmissionHandoffPhase::InvocationPrimary, &armed)
                .await,
            Err(DbError::InvalidTransition(_))
        ));

        // The second generation acknowledges (idempotently repeated).
        repo.record_generation_ack(1, &armed[1]).await.unwrap();
        repo.record_generation_ack(1, &armed[1]).await.unwrap();
        assert!(repo.generation_ack_complete(1, &armed).await.unwrap());

        // A stale-epoch generation acknowledgement is rejected.
        assert!(matches!(
            repo.record_generation_ack(0, "warm_build/alpha/0").await,
            Err(DbError::InvalidTransition(_))
        ));

        // With the full armed set acknowledged the edge commits exactly once.
        let primary = repo
            .advance(1, AdmissionHandoffPhase::InvocationPrimary, &armed)
            .await
            .unwrap();
        assert_eq!(primary.phase, AdmissionHandoffPhase::InvocationPrimary);
        assert_eq!(primary.epoch, 2);

        // A second attempt from the now-stale epoch fails.
        assert!(matches!(
            repo.advance(1, AdmissionHandoffPhase::InvocationPrimary, &armed)
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        // Advancing retired the epoch-1 acknowledgements.
        assert!(
            !repo.generation_ack_complete(1, &armed).await.unwrap(),
            "advancing retired the epoch-1 generation acknowledgements"
        );
    }
}
