//! The durable arming authority for the per-invocation cgroup CPU lease.
//!
//! # What this is
//!
//! One singleton row that answers exactly two questions:
//!
//! 1. **Is the per-invocation cgroup CPU lease armed?** ([`Self::mode`]) — the
//!    operator's kill switch. `Off` means no invocation is leased, `Shadow`
//!    means bind-and-measure without lifting, `Enforce` means a bound
//!    invocation with a matching fencing token may lift `cpu.max`.
//! 2. **What reference cap does the build-slot FIFO enforce?**
//!    ([`InvocationLeaseAuthorityRow::cap`]) — adopted at runtime by
//!    `BuildLeaseService`, so `djinn-server epoch set-cap` moves the cap the
//!    process is actually enforcing without a restart.
//!
//! `epoch` is a compare-and-swap fence and nothing more: it serializes
//! concurrent operator writes on one row lock so two mutations cannot interleave
//! into a contradictory committed state. **It is not an acknowledgement
//! protocol.** No reader waits on it, and no reader can be disarmed by it.
//!
//! # What it replaced, and why the replacement is smaller
//!
//! This relation used to be the `admission_handoff`: a two-authority handoff
//! state machine coordinating a v0 "emergency" ledger authority with the v1
//! "invocation" authority across a four-phase ring
//! (`emergency_primary → forward_overlap → invocation_primary →
//! rollback_overlap`), gated by per-authority acknowledgements
//! (`emergency_ack_epoch`, `invocation_ack_epoch`) and per-generation
//! acknowledgements in a companion table.
//!
//! The Kueue cutover (proposal `9oga`) deleted the v0 authority. All of that
//! machinery existed to hand capacity between two authorities; with one
//! authority left there is nothing to hand over, so:
//!
//! - **The phase is gone.** Every phase distinction was "which authority is
//!   primary".
//! - **The acknowledgements are gone, not collapsed.** An acknowledgement is a
//!   field that a writer must keep current or the reader fails closed. The v0
//!   ack had exactly one writer, and deleting that writer would have silently
//!   dropped every invocation to `Unleased` — no quota of its own, no
//!   containment — at the first `epoch advance` an operator ran, with no compile
//!   error and no failing test. Slice S3a bridged it by writing the ack
//!   unconditionally. Collapsing onto `invocation_ack_epoch` instead would have
//!   reproduced the same shape one column over: that ack has no runtime writer
//!   either — only the operator executor writes it, as part of its own
//!   transitions — so any epoch bump from outside the executor would disarm the
//!   lease exactly as the emergency ack did. An always-true acknowledgement is
//!   not a safety property; it is a field that can only ever be wrong. Both are
//!   removed, and the arming decision now reads the mode alone.
//!
//! # Physical storage
//!
//! The row still physically lives in the `admission_handoff` table under
//! `name = 'build'`, and the mode is still the `v1_mode` column. The retired
//! protocol columns (`phase`, `v0_mode`, `emergency_ack_epoch`,
//! `invocation_ack_epoch`) and the `admission_handoff_generation_ack` table are
//! still present and are dropped by `flc5`'s migration, which is deliberately a
//! separate change: the code stops depending on them first, so the DROP cannot
//! be what disarms production.
//!
//! Nothing here writes a retired column except [`Self::seed_baseline`], which
//! must supply the `NOT NULL` `phase` to create a row at all — see that method.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

/// The physical singleton key. One authority row, addressable by one name.
const AUTHORITY_NAME: &str = "build";

/// Only the columns the invocation lease authority still owns. The retired
/// handoff-protocol columns are deliberately absent: selecting them is what
/// would make `flc5`'s DROP migration a breaking change.
const AUTHORITY_COLUMNS: &str = "name, epoch, v1_mode, cap, updated_at::text";

/// Whether, and how hard, the per-invocation cgroup CPU lease is armed.
///
/// This is the operator's kill switch, and the only input to the arming
/// decision (`djinn_supervisor::services::evaluate_invocation_lift`).
///
/// # `Shadow` CLAMPS — it does not speed anything up
///
/// Only `Enforce` ever raises `cpu.max`. `Shadow` binds the invocation, records
/// what enforcement *would* have done, and leaves the leaf pinned at the
/// broker's unleased quota for the whole command. Arming shadow therefore makes
/// every leased build slower, not faster; it is an observation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationLeaseMode {
    /// The authority is disarmed: no invocation is leased.
    Off,
    /// Bind and measure, but never lift the reserved quota.
    Shadow,
    /// A bound invocation with a matching fencing token may lift `cpu.max`.
    Enforce,
}

impl InvocationLeaseMode {
    /// The durable spelling. These are the three values the
    /// `admission_handoff_v1_mode_check` constraint permits.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }

    /// Parse a durable spelling. Unknown values are rejected rather than
    /// defaulted: a mode nobody can name must not silently read as armed OR as
    /// disarmed.
    pub fn parse(value: &str) -> DbResult<Self> {
        match value {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "enforce" => Ok(Self::Enforce),
            _ => Err(DbError::InvalidData(format!(
                "invalid invocation lease mode `{value}`"
            ))),
        }
    }

    /// Whether this mode actually enforces the reference cap and lifts quota.
    /// This is the verdict `board_health`'s `lease_authority_enforcing` reports.
    #[must_use]
    pub const fn is_enforcing(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// The single durable invocation-lease authority record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationLeaseAuthorityRow {
    /// Compare-and-swap fence for operator writes. NOT an acknowledgement
    /// protocol — see the module docs.
    pub epoch: i64,
    /// The arming switch.
    pub mode: InvocationLeaseMode,
    /// Reference concurrency cap; `None` defers to the process configuration.
    pub cap: Option<i64>,
    /// RFC3339-formatted by PostgreSQL.
    pub updated_at: String,
}

/// Transactional Postgres repository for the invocation-lease authority
/// singleton.
pub struct InvocationLeaseAuthorityRepository {
    db: Database,
}

impl InvocationLeaseAuthorityRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Read the singleton without taking a write lock.
    ///
    /// `None` is a meaningful result, not an error: a deployment that has never
    /// seeded the authority is legitimately disarmed, and the arming decision
    /// maps it to `Unleased`.
    pub async fn read(&self) -> DbResult<Option<InvocationLeaseAuthorityRow>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, AuthorityDbRow>(&format!(
            "SELECT {AUTHORITY_COLUMNS} FROM admission_handoff WHERE name = $1"
        ))
        .bind(AUTHORITY_NAME)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Create the singleton at its DISARMED baseline.
    ///
    /// Seeding is idempotent: an already present row is returned untouched, so
    /// this can never overwrite a live rollout state. Startup deliberately never
    /// re-creates an absent row — removing it is the documented remediation for
    /// a wedged authority — so restoring it is an explicit operator action.
    ///
    /// `phase` is written here, and only here, because migration 129 declared it
    /// `NOT NULL` with no default and `flc5` has not dropped it yet. The value
    /// is `emergency_primary` specifically because that is the one phase an
    /// older binary rolled back over this seed reads as "not armed": a rollback
    /// across a fresh seed then fails closed rather than lifting quota under a
    /// protocol this binary no longer runs.
    pub async fn seed_baseline(&self) -> DbResult<InvocationLeaseAuthorityRow> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO admission_handoff (name, phase, epoch, v1_mode) \
             VALUES ($1, 'emergency_primary', 0, 'off') \
             ON CONFLICT (name) DO NOTHING",
        )
        .bind(AUTHORITY_NAME)
        .execute(self.db.pool())
        .await?;
        self.read().await?.ok_or_else(|| {
            DbError::InvalidData("invocation lease authority absent after seeding".into())
        })
    }

    /// Compare-and-swap the arming mode and the reference cap at the current
    /// epoch.
    ///
    /// The read-modify-write is serialized on the singleton's row lock and
    /// fenced on `expected_epoch`, so two operators racing a mode change and a
    /// cap change cannot interleave: whichever commits first bumps the epoch and
    /// the other is rejected as stale.
    ///
    /// The retired acknowledgement columns are deliberately NOT cleared here.
    /// Nothing in this binary reads them, and leaving them behind at their old
    /// epoch is the fail-closed choice for a rollback: an older binary compares
    /// them against the bumped epoch, finds them stale, and keeps the quota
    /// unleased rather than lifting it.
    pub async fn set_mode_and_cap(
        &self,
        expected_epoch: i64,
        mode: InvocationLeaseMode,
        cap: Option<i64>,
    ) -> DbResult<InvocationLeaseAuthorityRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx).await?;
        if row.epoch != expected_epoch {
            return Err(DbError::InvalidTransition(format!(
                "stale invocation lease authority epoch {expected_epoch}; current epoch is {}",
                row.epoch
            )));
        }
        let updated = sqlx::query_as::<_, AuthorityDbRow>(&format!(
            "UPDATE admission_handoff \
             SET v1_mode = $1, cap = $2, epoch = epoch + 1, updated_at = now() \
             WHERE name = $3 AND epoch = $4 RETURNING {AUTHORITY_COLUMNS}"
        ))
        .bind(mode.as_str())
        .bind(cap)
        .bind(AUTHORITY_NAME)
        .bind(expected_epoch)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        updated.try_into()
    }

    /// Write the **verbatim live production row**, retired columns and all.
    ///
    /// This is the behaviour-preservation fixture for the Kueue cutover's S3b
    /// slice. It is not a synthetic "armed epoch": it is the exact singleton
    /// `kubectl -n djinn exec deploy/djinn-server -- djinn-server epoch show`
    /// printed on 2026-07-30, column for column:
    ///
    /// ```text
    /// phase                 ForwardOverlap
    /// epoch                 14
    /// v0_mode               Enforce
    /// v1_mode               Enforce
    /// cap                   3
    /// emergency_ack_epoch   14
    /// invocation_ack_epoch  14
    /// ```
    ///
    /// It is written by raw SQL rather than by walking the repository's own
    /// mutations ON PURPOSE. Every column is stated as a literal, so the fixture
    /// keeps describing the row that is durably in production even though this
    /// code no longer writes — and no longer selects — the retired
    /// handoff-protocol columns. A fixture reconstructed from the surviving
    /// primitives would silently follow the code and stop proving anything about
    /// the live row.
    ///
    /// Until `flc5`'s DROP migration retires those columns, a deployed binary
    /// must read this exact physical row and still arm the per-invocation cgroup
    /// CPU lease.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn seed_live_production_row_for_test(&self) -> DbResult<InvocationLeaseAuthorityRow> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO admission_handoff \
                 (name, phase, epoch, emergency_ack_epoch, invocation_ack_epoch, \
                  v0_mode, v1_mode, cap) \
             VALUES ($1, 'forward_overlap', 14, 14, 14, 'enforce', 'enforce', 3) \
             ON CONFLICT (name) DO UPDATE SET \
                 phase = EXCLUDED.phase, \
                 epoch = EXCLUDED.epoch, \
                 emergency_ack_epoch = EXCLUDED.emergency_ack_epoch, \
                 invocation_ack_epoch = EXCLUDED.invocation_ack_epoch, \
                 v0_mode = EXCLUDED.v0_mode, \
                 v1_mode = EXCLUDED.v1_mode, \
                 cap = EXCLUDED.cap",
        )
        .bind(AUTHORITY_NAME)
        .execute(self.db.pool())
        .await?;
        self.read().await?.ok_or_else(|| {
            DbError::InvalidData("live production fixture absent after seeding".into())
        })
    }

    /// Remove the singleton to exercise the behaviour of an installation that
    /// has never armed the authority.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn delete_for_test(&self) -> DbResult<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM admission_handoff WHERE name = $1")
            .bind(AUTHORITY_NAME)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct AuthorityDbRow {
    name: String,
    epoch: i64,
    v1_mode: String,
    cap: Option<i64>,
    updated_at: String,
}

impl TryFrom<AuthorityDbRow> for InvocationLeaseAuthorityRow {
    type Error = DbError;

    fn try_from(value: AuthorityDbRow) -> Result<Self, Self::Error> {
        if value.name != AUTHORITY_NAME {
            return Err(DbError::InvalidData(format!(
                "invalid invocation lease authority singleton `{}`",
                value.name
            )));
        }
        if value.epoch < 0 {
            return Err(DbError::InvalidData(
                "negative invocation lease authority epoch".into(),
            ));
        }
        Ok(Self {
            epoch: value.epoch,
            mode: InvocationLeaseMode::parse(&value.v1_mode)?,
            cap: value.cap,
            updated_at: value.updated_at,
        })
    }
}

async fn current_row_for_update(
    tx: &mut Transaction<'_, Postgres>,
) -> DbResult<InvocationLeaseAuthorityRow> {
    let row = sqlx::query_as::<_, AuthorityDbRow>(&format!(
        "SELECT {AUTHORITY_COLUMNS} FROM admission_handoff WHERE name = $1 FOR UPDATE"
    ))
    .bind(AUTHORITY_NAME)
    .fetch_optional(&mut **tx)
    .await?;
    row.ok_or_else(|| {
        DbError::InvalidTransition("invocation lease authority singleton is absent".into())
    })?
    .try_into()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repository() -> InvocationLeaseAuthorityRepository {
        InvocationLeaseAuthorityRepository::new(Database::open_in_memory().unwrap())
    }

    #[tokio::test]
    async fn the_migration_seeded_singleton_starts_disarmed() {
        let repo = repository().await;
        let row = repo.read().await.unwrap().unwrap();
        assert_eq!(row.epoch, 0);
        assert_eq!(
            row.mode,
            InvocationLeaseMode::Off,
            "a deployment that has never armed the authority leases nothing"
        );
        assert_eq!(row.cap, None);
    }

    /// Re-seeding a deployment whose row was deleted (the documented wedge
    /// remediation) must land DISARMED and must never disturb an existing row.
    #[tokio::test]
    async fn seeding_recreates_a_disarmed_baseline_and_never_overwrites() {
        let repo = repository().await;
        repo.delete_for_test().await.unwrap();
        assert!(repo.read().await.unwrap().is_none());

        let seeded = repo.seed_baseline().await.unwrap();
        assert_eq!(seeded.epoch, 0);
        assert_eq!(seeded.mode, InvocationLeaseMode::Off);
        assert_eq!(seeded.cap, None);

        // An armed rollout must survive a repeated seed untouched.
        let armed = repo
            .set_mode_and_cap(seeded.epoch, InvocationLeaseMode::Enforce, Some(5))
            .await
            .unwrap();
        let reseeded = repo.seed_baseline().await.unwrap();
        assert_eq!(reseeded, armed, "seeding is idempotent, never destructive");
    }

    #[tokio::test]
    async fn mode_and_cap_round_trip_and_the_epoch_fences_stale_writers() {
        let repo = repository().await;
        let seeded = repo.read().await.unwrap().unwrap();

        let armed = repo
            .set_mode_and_cap(seeded.epoch, InvocationLeaseMode::Enforce, Some(7))
            .await
            .unwrap();
        assert_eq!(armed.mode, InvocationLeaseMode::Enforce);
        assert_eq!(armed.cap, Some(7));
        assert_eq!(armed.epoch, seeded.epoch + 1);

        // A stale-epoch write is rejected and leaves the row untouched.
        assert!(matches!(
            repo.set_mode_and_cap(seeded.epoch, InvocationLeaseMode::Off, None)
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        // The cap CHECK constraint still rejects a non-positive cap.
        assert!(
            repo.set_mode_and_cap(armed.epoch, InvocationLeaseMode::Enforce, Some(0))
                .await
                .is_err()
        );
        let current = repo.read().await.unwrap().unwrap();
        assert_eq!(current, armed, "the rejected writes changed nothing");
    }

    /// The epoch is a CAS fence, and that is the whole of its job: with a paused
    /// transaction holding the singleton's row lock, two epoch-0 mutations
    /// queue; when it is released exactly one applies and the other is rejected
    /// as stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_operator_writes_from_the_same_epoch_cannot_both_apply() {
        let db = Database::open_in_memory().unwrap();
        let repo = std::sync::Arc::new(InvocationLeaseAuthorityRepository::new(db.clone()));
        // Force the template clone / schema initialization before the manual lock.
        assert_eq!(repo.read().await.unwrap().unwrap().epoch, 0);

        let mut hold = db.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM admission_handoff WHERE name = 'build' FOR UPDATE")
            .fetch_one(&mut *hold)
            .await
            .unwrap();

        let arm = {
            let repo = std::sync::Arc::clone(&repo);
            tokio::spawn(async move {
                repo.set_mode_and_cap(0, InvocationLeaseMode::Enforce, Some(3))
                    .await
            })
        };
        let recap = {
            let repo = std::sync::Arc::clone(&repo);
            tokio::spawn(async move {
                repo.set_mode_and_cap(0, InvocationLeaseMode::Shadow, Some(4))
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        hold.rollback().await.unwrap();

        let arm = arm.await.unwrap();
        let recap = recap.await.unwrap();
        assert_eq!(
            [arm.is_ok(), recap.is_ok()]
                .into_iter()
                .filter(|ok| *ok)
                .count(),
            1,
            "exactly one epoch-0 mutation applies"
        );
        assert_eq!(
            repo.read().await.unwrap().unwrap().epoch,
            1,
            "the winning mutation advanced the epoch exactly once"
        );
    }

    /// An unknown durable mode is rejected rather than defaulted. Defaulting to
    /// `off` would silently disarm containment; defaulting to `enforce` would
    /// silently arm it. Neither is a safe reading of a value nobody wrote.
    #[test]
    fn an_unknown_durable_mode_is_a_read_error() {
        assert!(InvocationLeaseMode::parse("enforce").is_ok());
        assert!(matches!(
            InvocationLeaseMode::parse("emergency_primary"),
            Err(DbError::InvalidData(_))
        ));
    }

    /// The live production row is readable through the reduced column set: the
    /// authority selects only the columns it still owns, so the retired
    /// protocol columns can be dropped by `flc5` without breaking this read.
    #[tokio::test]
    async fn the_live_production_row_reads_as_an_armed_authority_at_cap_3() {
        let repo = repository().await;
        let row = repo.seed_live_production_row_for_test().await.unwrap();
        assert_eq!(row.epoch, 14);
        assert_eq!(row.mode, InvocationLeaseMode::Enforce);
        assert_eq!(row.cap, Some(3));
    }
}
