//! The server-wide launcher quota-authority mode, and the drain fence that
//! guards every change to it.
//!
//! # What this is
//!
//! One singleton row (migration 167) naming the single component permitted to
//! write task-run CPU quota:
//!
//! * [`LauncherAuthorityProtocol::LeafV1`] — the launcher owns the birth and
//!   fenced lift writes on its per-invocation cgroup leaf.
//! * [`LauncherAuthorityProtocol::ResizeV2`] — Kubernetes in-place Pod resize
//!   owns the launcher sidecar's `limits.cpu`, and the launcher must never
//!   write leaf `cpu.max`.
//!
//! There is no mode admitting both writers, and none admitting neither. That is
//! the whole point: [`admit_declared_protocol`] refuses a Pod whose image
//! declares the other protocol *before* it can dispatch a shell, so a Pod never
//! runs with two quota authorities or with zero.
//!
//! # Why the flip needs a fence at all
//!
//! A running Pod's quota authority is fixed at the moment its image was built
//! and its identity captured. Flipping the server mode under a live Pod does
//! not migrate it — it strands it: a `leaf-v1` Pod under `resize-v2` authority
//! has a launcher that already wrote its leaf quota and a server that believes
//! Kubernetes owns it, and a `resize-v2` Pod under `leaf-v1` authority has
//! nobody writing quota at all. So a flip is only sound from a fully drained
//! state, and "drained" must be established transactionally rather than
//! observed and hoped for.
//!
//! # Why the count cannot race
//!
//! [`LauncherAuthorityModeRepository::set_mode`] takes the SAME
//! `build_pod_permit_pools` row lock that
//! [`crate::BuildPodPermitRepository::acquire`] takes before it counts and
//! inserts. While the flip transaction is open, no new permit row can come into
//! existence: a concurrent admission blocks on that lock and only proceeds once
//! the flip has committed or rolled back. A zero-count observation therefore
//! cannot be invalidated by a row that appears a microsecond later — which is
//! the exact shape of the 2026-07-25 wedge in the neighbouring admission
//! subsystem, where a settle-window read that could not distinguish "measured
//! empty" from "not measured" was treated as empty.
//!
//! Existing rows can still change state under the lock (a capture moves
//! `job_created` → `birth_confirmed`), so the census is a SINGLE statement over
//! one snapshot. Two sequential counts could otherwise see a row in neither
//! bucket, or in both.
//!
//! # Reads and errors are never empty
//!
//! Every failure mode in this module has its own outcome and none of them is
//! "drained":
//!
//! | situation | outcome |
//! |---|---|
//! | singleton row absent | [`SetLauncherAuthorityModeResult::Uninitialized`] |
//! | pool row / relation missing, query failed, lock timed out | [`SetLauncherAuthorityModeResult::Unavailable`] |
//! | stale CAS epoch | [`SetLauncherAuthorityModeResult::EpochConflict`] |
//! | either drain dimension nonzero | [`SetLauncherAuthorityModeResult::DrainNotEmpty`] |
//!
//! [`LauncherAuthorityModeRepository::drain_census`] returns `DbResult` rather
//! than a census, so a caller cannot accidentally read a failed count as zero.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use djinn_launcher_protocol::LauncherAuthorityProtocol;

use crate::database::Database;
use crate::error::{DbError, DbResult};
use crate::repositories::build_pod_permit::{NONTERMINAL_RESIZE_STATES, sql_state_list};

/// The physical singleton key, guarded by `launcher_authority_mode_singleton_check`.
const MODE_KEY: &str = "global";

/// The `build_pod_permit_pools` key that migration 162 declares and
/// [`crate::BuildPodPermitRepository::acquire`] locks. Restated here because
/// the fence is only real if both transactions contend on the *same* row.
const PERMIT_POOL_KEY: &str = "global";

const MODE_COLUMNS: &str = "mode, epoch, updated_at::text";

/// The durable authority-mode record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherAuthorityModeRow {
    /// The one component permitted to write task-run CPU quota.
    pub mode: LauncherAuthorityProtocol,
    /// Compare-and-swap fence for operator writes.
    pub epoch: i64,
    /// RFC3339-formatted by PostgreSQL.
    pub updated_at: String,
}

/// A bounded, per-dimension census of everything that blocks an authority flip.
///
/// The two dimensions partition the active permits exactly — every permit that
/// is not `released` falls in precisely one — so `pending + nonterminal` is the
/// total live occupancy and a future lifecycle state cannot land outside both.
///
/// This is the typed diagnostic the operational preflight (`xowm`) consumes: it
/// says not just *that* a flip is blocked but *which* dimension blocks it, so
/// "a Pod is still scheduled" and "a Pod still owes a drop" are distinguishable
/// without a second query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherAuthorityDrainCensus {
    /// Permits holding capacity without a resize lifecycle: `acquired` and
    /// `job_created`. A live or pending task-run Pod whose identity has not
    /// been captured.
    pub pending_pod_permits: i64,
    /// Permits in one of the six nonterminal resize/lease lifecycle states from
    /// migration 164, i.e. a Pod that may still be lifted or still owes a drop.
    pub nonterminal_resize_leases: i64,
}

impl LauncherAuthorityDrainCensus {
    /// Total live occupancy across both dimensions.
    #[must_use]
    pub const fn total(self) -> i64 {
        self.pending_pod_permits + self.nonterminal_resize_leases
    }

    /// Both dimensions empty. Deliberately an `AND` over the dimensions rather
    /// than `total() == 0` on a single number, so the predicate reads the same
    /// way the requirement is stated and a future third dimension has an
    /// obvious place to be conjoined.
    #[must_use]
    pub const fn is_drained(self) -> bool {
        self.pending_pod_permits == 0 && self.nonterminal_resize_leases == 0
    }
}

/// The outcome of one guarded authority-mode compare-and-swap.
///
/// Only [`Self::Flipped`] and [`Self::Unchanged`] mean the requested mode is in
/// effect, and both are only reachable from a transactionally fenced, fully
/// drained snapshot at the expected epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetLauncherAuthorityModeResult {
    /// The mode changed. `previous` is what it changed from.
    Flipped {
        row: LauncherAuthorityModeRow,
        previous: LauncherAuthorityProtocol,
        drain: LauncherAuthorityDrainCensus,
    },
    /// Same-mode replay. Idempotent — no write, no epoch bump — but reached
    /// only after the same epoch check and the same drain fence a real flip
    /// passes, so it cannot be used to launder a flip past a live Pod.
    Unchanged {
        row: LauncherAuthorityModeRow,
        drain: LauncherAuthorityDrainCensus,
    },
    /// At least one drain dimension is nonzero. The mode is unchanged and
    /// `drain` names which dimension refused.
    DrainNotEmpty {
        row: LauncherAuthorityModeRow,
        drain: LauncherAuthorityDrainCensus,
    },
    /// `expected_epoch` did not match. `row` carries the epoch that did.
    EpochConflict { row: LauncherAuthorityModeRow },
    /// The singleton row is absent. Never treated as a default mode.
    Uninitialized,
    /// A read, a lock, or the census failed. Never treated as drained.
    Unavailable,
}

/// The admission verdict for one Pod's declared launcher protocol.
///
/// Only [`Self::Admitted`] may dispatch a shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LauncherProtocolAdmission {
    /// The declared protocol is exactly the configured authority.
    Admitted { mode: LauncherAuthorityProtocol },
    /// The image explicitly declares the *other* protocol. Admitting it would
    /// give the Pod two quota writers or none.
    ProtocolMismatch {
        mode: LauncherAuthorityProtocol,
        declared: LauncherAuthorityProtocol,
    },
    /// The stored declaration is not one of the two wire forms. Refused rather
    /// than defaulted: [`LauncherAuthorityProtocol`] has no fallback arm
    /// precisely because guessing is an outage in one direction and a silent
    /// 16x slowdown in the other.
    UnknownProtocol {
        mode: LauncherAuthorityProtocol,
        declared: String,
    },
    /// A legacy image built before the handshake existed. Whether an exact,
    /// signed, immutable digest may map "no declaration" to `leaf-v1` is the
    /// legacy digest allowlist, owned by `vf7a`. Until that lands this is a
    /// refusal in BOTH modes, never an admission: an unlisted no-handshake
    /// image under `resize-v2` runs with no quota authority at all.
    UndeclaredProtocol { mode: LauncherAuthorityProtocol },
    /// The authority mode could not be read. Never resolved to a default.
    AuthorityUnavailable,
}

impl LauncherProtocolAdmission {
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted { .. })
    }
}

/// Decide one admission against an already-read authority mode.
///
/// Split out from the repository so the decision is a total, side-effect-free
/// function over its two inputs: the same verdict is reachable from a preflight
/// that has already read the mode without spending a second round trip, and it
/// is exhaustively testable without a database.
#[must_use]
pub fn decide_launcher_protocol_admission(
    mode: LauncherAuthorityProtocol,
    declared: Option<&str>,
) -> LauncherProtocolAdmission {
    let Some(declared) = declared else {
        return LauncherProtocolAdmission::UndeclaredProtocol { mode };
    };
    match declared.parse::<LauncherAuthorityProtocol>() {
        Ok(declared) if declared == mode => LauncherProtocolAdmission::Admitted { mode },
        Ok(declared) => LauncherProtocolAdmission::ProtocolMismatch { mode, declared },
        Err(_) => LauncherProtocolAdmission::UnknownProtocol {
            mode,
            declared: declared.to_owned(),
        },
    }
}

/// Transactional Postgres repository for the authority-mode singleton.
pub struct LauncherAuthorityModeRepository {
    db: Database,
}

impl LauncherAuthorityModeRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Read the singleton without taking a write lock.
    ///
    /// `Ok(None)` means the row is absent, which is not a mode. An unreadable
    /// relation is an `Err`, never `None`: the two must stay distinguishable or
    /// a caller would fail closed on one and default on the other.
    pub async fn read(&self) -> DbResult<Option<LauncherAuthorityModeRow>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query_as::<_, ModeDbRow>(&format!(
            "SELECT {MODE_COLUMNS} FROM launcher_authority_mode WHERE mode_key = $1"
        ))
        .bind(MODE_KEY)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// Count both drain dimensions in one snapshot, without locking.
    ///
    /// This is the read-only diagnostic for preflight and board health. It is
    /// deliberately NOT the fence: an unlocked census is a report about the
    /// past, and only [`Self::set_mode`] — which holds the permit pool lock
    /// while it counts — may authorize a flip from one.
    pub async fn drain_census(&self) -> DbResult<LauncherAuthorityDrainCensus> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let census = drain_census_tx(&mut tx).await?;
        tx.commit().await?;
        Ok(census)
    }

    /// Compare-and-swap the authority mode behind an empty durable drain fence.
    ///
    /// The transaction, in order:
    ///
    /// 1. locks the authority singleton `FOR UPDATE`, serializing operators;
    /// 2. rejects a stale `expected_epoch`;
    /// 3. locks the `build_pod_permit_pools` singleton `FOR UPDATE` — the same
    ///    row [`crate::BuildPodPermitRepository::acquire`] locks before it
    ///    counts and inserts, which is what makes the census below unraceable;
    /// 4. counts both drain dimensions in ONE statement;
    /// 5. refuses unless BOTH are zero;
    /// 6. only then writes, bumping the epoch.
    ///
    /// **Lock order is authority row, then permit pool row, and never the
    /// reverse.** `acquire` takes only the pool lock, so no cycle exists today;
    /// a future writer that needs both must take them in this order.
    ///
    /// The same-mode replay check happens at step 5b, *after* the fence, so an
    /// operator cannot use a no-op replay to skip the validation a real flip
    /// requires.
    ///
    /// Errors collapse into [`SetLauncherAuthorityModeResult::Unavailable`]
    /// with the cause logged, matching `acquire`'s fail-closed shape: there is
    /// no successful fallback for unavailable durable storage.
    pub async fn set_mode(
        &self,
        expected_epoch: i64,
        next: LauncherAuthorityProtocol,
    ) -> SetLauncherAuthorityModeResult {
        match self.set_mode_inner(expected_epoch, next).await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    requested_mode = next.as_wire(),
                    expected_epoch,
                    "launcher authority mode flip unavailable; mode left unchanged"
                );
                SetLauncherAuthorityModeResult::Unavailable
            }
        }
    }

    async fn set_mode_inner(
        &self,
        expected_epoch: i64,
        next: LauncherAuthorityProtocol,
    ) -> DbResult<SetLauncherAuthorityModeResult> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;

        let current = sqlx::query_as::<_, ModeDbRow>(&format!(
            "SELECT {MODE_COLUMNS} FROM launcher_authority_mode WHERE mode_key = $1 FOR UPDATE"
        ))
        .bind(MODE_KEY)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current) = current else {
            tx.commit().await?;
            return Ok(SetLauncherAuthorityModeResult::Uninitialized);
        };
        let current: LauncherAuthorityModeRow = current.try_into()?;
        if current.epoch != expected_epoch {
            tx.commit().await?;
            return Ok(SetLauncherAuthorityModeResult::EpochConflict { row: current });
        }

        // The fence. Admission cannot insert a permit while this is held.
        let pool: Option<String> = sqlx::query_scalar(
            "SELECT pool_key FROM build_pod_permit_pools WHERE pool_key = $1 FOR UPDATE",
        )
        .bind(PERMIT_POOL_KEY)
        .fetch_optional(&mut *tx)
        .await?;
        if pool.is_none() {
            // A missing pool means admission is not serialized by anything this
            // transaction holds, so the census below would be unfenced. That is
            // an unavailable fence, not an empty one. `acquire` reaches the same
            // verdict from the same observation, and for the same reason.
            return Err(DbError::Internal(
                "build pod permit global pool is missing; the authority drain fence is unavailable"
                    .into(),
            ));
        }

        // `?`, not a default. A failed count is not a count of zero.
        //
        // PostgreSQL happens to defend this a second time — a statement error
        // aborts the surrounding transaction, so the `UPDATE` below could not
        // commit even if this propagation were removed — but that is a property
        // of the engine, not of this function, and it disappears the moment the
        // census is hoisted out of the flip transaction. Keep the `?`.
        let drain = drain_census_tx(&mut tx).await?;
        if !drain.is_drained() {
            tx.commit().await?;
            return Ok(SetLauncherAuthorityModeResult::DrainNotEmpty {
                row: current,
                drain,
            });
        }

        if current.mode == next {
            tx.commit().await?;
            return Ok(SetLauncherAuthorityModeResult::Unchanged {
                row: current,
                drain,
            });
        }

        let updated = sqlx::query_as::<_, ModeDbRow>(&format!(
            "UPDATE launcher_authority_mode \
             SET mode = $1, epoch = epoch + 1, updated_at = now() \
             WHERE mode_key = $2 AND epoch = $3 RETURNING {MODE_COLUMNS}"
        ))
        .bind(next.as_wire())
        .bind(MODE_KEY)
        .bind(expected_epoch)
        .fetch_one(&mut *tx)
        .await?;
        let row: LauncherAuthorityModeRow = updated.try_into()?;
        tx.commit().await?;
        Ok(SetLauncherAuthorityModeResult::Flipped {
            row,
            previous: current.mode,
            drain,
        })
    }

    /// Admit or refuse a Pod's declared launcher protocol under the configured
    /// authority. This is the pre-dispatch rejection point.
    ///
    /// A missing singleton row and an unreadable relation both yield
    /// [`LauncherProtocolAdmission::AuthorityUnavailable`], which is a refusal.
    pub async fn admit_declared_protocol(
        &self,
        declared: Option<&str>,
    ) -> LauncherProtocolAdmission {
        match self.read().await {
            Ok(Some(row)) => decide_launcher_protocol_admission(row.mode, declared),
            Ok(None) => {
                tracing::warn!("launcher authority mode is unseeded; refusing protocol admission");
                LauncherProtocolAdmission::AuthorityUnavailable
            }
            Err(error) => {
                tracing::warn!(error = %error, "launcher authority mode unreadable; refusing protocol admission");
                LauncherProtocolAdmission::AuthorityUnavailable
            }
        }
    }
}

/// Both drain dimensions from one snapshot of `build_pod_permits`.
///
/// `state <> 'released'` is the active predicate migration 162 established;
/// the `FILTER` split is the migration-164 nonterminal resize set against its
/// complement, so the two counts partition the active rows exactly.
async fn drain_census_tx(
    tx: &mut Transaction<'_, Postgres>,
) -> DbResult<LauncherAuthorityDrainCensus> {
    let nonterminal = sql_state_list(&NONTERMINAL_RESIZE_STATES);
    let (pending, resize): (i64, i64) = sqlx::query_as(&format!(
        "SELECT \
           count(*) FILTER (WHERE state NOT IN ({nonterminal}))::bigint, \
           count(*) FILTER (WHERE state IN ({nonterminal}))::bigint \
         FROM build_pod_permits WHERE state <> 'released'"
    ))
    .fetch_one(&mut **tx)
    .await?;
    Ok(LauncherAuthorityDrainCensus {
        pending_pod_permits: pending,
        nonterminal_resize_leases: resize,
    })
}

#[derive(sqlx::FromRow)]
struct ModeDbRow {
    mode: String,
    epoch: i64,
    updated_at: String,
}

impl TryFrom<ModeDbRow> for LauncherAuthorityModeRow {
    type Error = DbError;

    fn try_from(row: ModeDbRow) -> DbResult<Self> {
        Ok(Self {
            // No fallback to the enum's `Default`. A value the migration's
            // CHECK should have made impossible is corruption, and reading it
            // as `leaf-v1` would silently re-arm the launcher under a server
            // that believes resize owns quota.
            mode: row.mode.parse().map_err(|_| {
                DbError::InvalidData(format!("invalid launcher authority mode `{}`", row.mode))
            })?,
            epoch: row.epoch,
            updated_at: row.updated_at,
        })
    }
}
