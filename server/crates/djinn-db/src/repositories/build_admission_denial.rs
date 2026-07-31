//! Durable build-admission denials (#2661 follow-up).
//!
//! # NO WRITER REMAINS (Kueue cutover, o53p)
//!
//! Read this before trusting anything below. The only producer of these rows
//! was `dispatch/task_dispatch.rs`'s `record_task_run_denial`, which rendered a
//! `DenialCause` from the pre-create `BuildAdmissionController`. Both are
//! deleted: Kueue's ClusterQueue decides build capacity now, and a Job it has
//! not admitted is queued by Kueue rather than denied by this process.
//!
//! So the relation, this repository, and the `build_admission_denial` block in
//! `board_health` are all still wired and still correct — they simply describe a
//! population that is now empty and can only shrink. That is honest (`null`
//! means "no recorded denial", which is true) rather than a false healthy
//! signal, which is why this was retained rather than ripped out mid-cutover.
//! Retiring it, together with migration 161, belongs to the follow-up slices
//! (`plcj` / `flc5`). Do NOT add a new writer here: a denial recorded against a
//! deleted authority would be the replayed-tombstone failure of #2661 in a new
//! location.
//!
//! # Why this existed
//!
//! `BuildAdmissionDecision::Denied` has carried a `DenialCause` since #2661.
//! It was consumed at exactly one site — a `tracing::info!` in
//! `dispatch/task_dispatch.rs` followed by a bare `Err(())` — and persisted
//! nowhere. On 2026-07-29 every coordinator tick for five hours logged
//! `cause: "controller_not_admitting"` while `board_health` reported
//! `unexplained` with an empty `reasons`, because the only component that knew
//! why nothing was dispatching discarded the answer immediately.
//!
//! # Why a persisted signal and not a live read
//!
//! The obvious alternative is an MCP tool that asks the running controller.
//! It does not work. The controller's readiness is process-local state on the
//! leader, and `mark_topology_ready` is reachable only from `become_leader`, so
//! a standby serving the call would answer `TopologyPending` every time — a
//! confident wrong answer, which is worse than the silence it replaces. The
//! process that actually made the decision is the only one that can report it,
//! so it writes it down.
//!
//! # Why this cannot become a new tombstone
//!
//! #2661 was itself a tombstone bug: a spent lease row was replayed forever and
//! every denial re-derived itself. A denial row that outlives its condition
//! would recreate that failure in a new table. Two defences:
//!
//! 1. [`BuildAdmissionDenialRepository::clear`] runs on the permitted path, so
//!    a consumer that gets admitted has no row at all; and
//! 2. every read carries `denied_at`, so a consumer left with a stale row can
//!    be aged out by the reader rather than believed.

use crate::database::Database;
use crate::error::DbResult;

/// One build-admission denial as the deciding process observed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionDenialRecord {
    /// Matches `build_leases.consumer_kind`; today only `task_dispatch`.
    pub consumer_kind: String,
    /// The task id for `task_dispatch`.
    pub consumer_id: String,
    /// `DenialCause` rendered by its `Display`: `at_capacity`,
    /// `controller_not_admitting` or `authority_unavailable`.
    pub cause: String,
    /// The closed readiness gate, when the deleted pre-create controller
    /// refused. This is the field that lived only in container logs during the
    /// 2026-07-29 outage. Nothing writes it any more — see the module docs.
    pub readiness: Option<String>,
    /// The capacity authority's own words, for `authority_unavailable`.
    pub detail: Option<String>,
    /// Occupancy as MEASURED, or `None` when the denial never consulted it.
    /// `None` must never be written as `0`.
    pub occupancy: Option<i64>,
    /// The cap that would have been enforced.
    pub cap: i64,
    /// The deciding process's admission epoch.
    pub server_epoch: String,
}

/// Upsert/clear the durable denial record for one dispatch consumer.
#[derive(Clone)]
pub struct BuildAdmissionDenialRepository {
    db: Database,
}

impl BuildAdmissionDenialRepository {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Record the latest denial for a consumer.
    ///
    /// `first_denied_at` is preserved across repeats so "denied since" measures
    /// the whole streak rather than the last tick, and `denial_count` counts
    /// the ticks. Both reset only when [`Self::clear`] removes the row.
    pub async fn record(&self, denial: &BuildAdmissionDenialRecord) -> DbResult<()> {
        sqlx::query(
            r#"INSERT INTO build_admission_denials
                   (consumer_kind, consumer_id, cause, readiness, detail,
                    occupancy, cap, server_epoch)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
               ON CONFLICT (consumer_kind, consumer_id) DO UPDATE
                  SET cause        = EXCLUDED.cause,
                      readiness    = EXCLUDED.readiness,
                      detail       = EXCLUDED.detail,
                      occupancy    = EXCLUDED.occupancy,
                      cap          = EXCLUDED.cap,
                      server_epoch = EXCLUDED.server_epoch,
                      denied_at    = now(),
                      denial_count = build_admission_denials.denial_count + 1"#,
        )
        .bind(&denial.consumer_kind)
        .bind(&denial.consumer_id)
        .bind(&denial.cause)
        .bind(denial.readiness.as_deref())
        .bind(denial.detail.as_deref())
        .bind(denial.occupancy)
        .bind(denial.cap)
        .bind(&denial.server_epoch)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Drop the denial record for a consumer that has just been admitted.
    ///
    /// Called on the permitted path. Without it this table becomes exactly the
    /// #2661 tombstone in a new location.
    pub async fn clear(&self, consumer_kind: &str, consumer_id: &str) -> DbResult<()> {
        sqlx::query(
            "DELETE FROM build_admission_denials \
             WHERE consumer_kind = $1 AND consumer_id = $2",
        )
        .bind(consumer_kind)
        .bind(consumer_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}
