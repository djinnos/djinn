//! Durable, fenced admission-authority handoff state.
//!
//! The handoff row is deliberately separate from the workload admission journal:
//! it records which authority may admit work, not the work that has been admitted.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

const HANDOFF_NAME: &str = "build";
const HANDOFF_COLUMNS: &str =
    "name, phase, epoch, emergency_ack_epoch, invocation_ack_epoch, updated_at::text";

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

/// The single durable build-admission handoff record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionHandoffRow {
    pub phase: AdmissionHandoffPhase,
    pub epoch: i64,
    pub emergency_ack_epoch: Option<i64>,
    pub invocation_ack_epoch: Option<i64>,
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

    /// Compare-and-swap from the current phase to its only legal successor.
    ///
    /// Each edge requires evidence at the current epoch. Entering an overlap
    /// requires the outgoing primary authority; leaving an overlap requires both
    /// authorities, so neither authority can be disabled without confirming that
    /// the other remains healthy for this exact epoch.
    pub async fn advance(
        &self,
        expected_epoch: i64,
        next_phase: AdmissionHandoffPhase,
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
            updated_at: value.updated_at,
        })
    }
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
            repo.advance(row.epoch, row.phase.legal_next())
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
            let advanced = repo.advance(row.epoch, next).await.unwrap();
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
                repo.advance(row.epoch, row.phase.legal_next()).await,
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
            repo.advance(row.epoch, AdmissionHandoffPhase::InvocationPrimary)
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        let advanced = repo
            .advance(row.epoch, AdmissionHandoffPhase::ForwardOverlap)
            .await
            .unwrap();
        assert!(matches!(
            repo.advance(row.epoch, AdmissionHandoffPhase::InvocationPrimary)
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
}
