//! Durable projection of Kueue Workload admission state onto task-runs.
//!
//! Written by `djinn-server`'s `kueue_workload_reconcile` Workload reflector,
//! which is the ONLY writer. See `migrations_postgres/165_kueue_workload_admission.sql`
//! for why this is a projection and not an authority.
//!
//! # The one property worth stating twice
//!
//! [`KueueWorkloadAdmissionRepository::apply`] returns whether it CHANGED
//! anything, and `transitions` counts changes rather than observations. A
//! Kubernetes watch is not a stream of deltas: every reconnect replays the full
//! current state of every object it is watching, so a projection that treated
//! each observation as an event would count one admission dozens of times over a
//! day of routine watch churn. The idempotence lives here, in SQL, rather than
//! in the reflector's memory, so it survives a leader failover that empties that
//! memory.

use crate::database::Database;
use crate::error::DbResult;

/// Column tuple of the projection SELECTs, in declaration order.
type AdmissionRow = (String, String, Option<String>, Option<String>, i64);

/// One task-run's Kueue admission state, as the reflector last observed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KueueWorkloadAdmissionRecord {
    pub task_run_id: String,
    /// `pending`, `admitted` or `finished`.
    pub admission: String,
    /// Kueue's own word for the state, when it gave one.
    pub reason: Option<String>,
    pub workload_name: Option<String>,
    /// Observed state CHANGES since the row was created.
    pub transitions: i64,
}

/// What one [`KueueWorkloadAdmissionRepository::apply`] call did.
///
/// The distinction is the whole contract: a caller cannot tell a real edge from
/// a watch replay by inspecting the resulting row, because the resulting row is
/// identical either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionApplied {
    /// No row existed; the Workload is newly observed.
    Recorded,
    /// The row existed in a different state and moved. `transitions` advanced.
    ///
    /// `previous` is carried because the DIRECTION is what a caller acts on: a
    /// move to `pending` from `admitted` is a quota eviction of a running build,
    /// while a move to `pending` from nothing is a build that has simply been
    /// queued. Collapsing them would make the reconciler interrupt task-runs
    /// that were never admitted in the first place.
    Transitioned { previous: String },
    /// The row already held this state. Only `observed_at` moved.
    Unchanged,
}

#[derive(Clone)]
pub struct KueueWorkloadAdmissionRepository {
    db: Database,
}

impl KueueWorkloadAdmissionRepository {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Record `admission` for `task_run_id`, reporting whether it was an edge.
    ///
    /// Idempotent on (task_run_id, admission): re-applying the same state
    /// refreshes `observed_at` and returns [`AdmissionApplied::Unchanged`]
    /// WITHOUT advancing `transitions`. That is what makes a watch resync free.
    ///
    /// The state is deliberately not forward-only. Kueue preempts admitted
    /// Workloads for quota and re-admits them later, so 'admitted' → 'pending'
    /// is a legal and load-bearing move; only a caller could know it was wrong,
    /// and no caller does.
    pub async fn apply(
        &self,
        task_run_id: &str,
        admission: &str,
        reason: Option<&str>,
        workload_name: Option<&str>,
    ) -> DbResult<AdmissionApplied> {
        self.db.ensure_initialized().await?;
        // The `prev` CTE reads the row as it stood BEFORE this statement (every
        // CTE in one statement sees the same snapshot), which is the only way to
        // answer "did this move?" — `RETURNING` can only see the new row, and a
        // new row that already held this state is byte-identical to one that
        // just reached it.
        //
        // `transitions` advances inside the same predicate, so the counter and
        // the reported outcome can never disagree.
        let previous: Option<String> = sqlx::query_scalar(
            r#"WITH prev AS (
                   SELECT admission FROM kueue_workload_admission WHERE task_run_id = $1
               ), upsert AS (
                   INSERT INTO kueue_workload_admission
                       (task_run_id, admission, reason, workload_name)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (task_run_id) DO UPDATE
                      SET admission     = EXCLUDED.admission,
                          reason        = EXCLUDED.reason,
                          workload_name = COALESCE(EXCLUDED.workload_name,
                                                   kueue_workload_admission.workload_name),
                          observed_at   = now(),
                          transitions   = kueue_workload_admission.transitions
                                        + CASE WHEN kueue_workload_admission.admission
                                                    IS DISTINCT FROM EXCLUDED.admission
                                               THEN 1 ELSE 0 END
                   RETURNING task_run_id
               )
               SELECT (SELECT admission FROM prev) FROM upsert"#,
        )
        .bind(task_run_id)
        .bind(admission)
        .bind(reason)
        .bind(workload_name)
        .fetch_one(self.db.pool())
        .await?;

        Ok(match previous {
            None => AdmissionApplied::Recorded,
            Some(prev) if prev == admission => AdmissionApplied::Unchanged,
            Some(previous) => AdmissionApplied::Transitioned { previous },
        })
    }

    pub async fn get(&self, task_run_id: &str) -> DbResult<Option<KueueWorkloadAdmissionRecord>> {
        self.db.ensure_initialized().await?;
        let row: Option<AdmissionRow> = sqlx::query_as(
            "SELECT task_run_id, admission, reason, workload_name, transitions
             FROM kueue_workload_admission WHERE task_run_id = $1",
        )
        .bind(task_run_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(
            |(task_run_id, admission, reason, workload_name, transitions)| {
                KueueWorkloadAdmissionRecord {
                    task_run_id,
                    admission,
                    reason,
                    workload_name,
                    transitions,
                }
            },
        ))
    }

    /// Drop the projection for a task-run whose Workload is gone.
    ///
    /// Kueue garbage-collects a Workload with its owning Job, so a surviving row
    /// would describe an object the cluster no longer has — the tombstone shape
    /// that #2661 cost five hours of dispatch.
    pub async fn forget(&self, task_run_id: &str) -> DbResult<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM kueue_workload_admission WHERE task_run_id = $1")
            .bind(task_run_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Task-runs currently waiting on Kueue quota, newest observation first.
    ///
    /// This is the operator answer to "why is the board sitting still" that the
    /// deleted pre-create ledger threw away.
    pub async fn pending(&self) -> DbResult<Vec<KueueWorkloadAdmissionRecord>> {
        self.db.ensure_initialized().await?;
        let rows: Vec<AdmissionRow> = sqlx::query_as(
            "SELECT task_run_id, admission, reason, workload_name, transitions
             FROM kueue_workload_admission
             WHERE admission = 'pending'
             ORDER BY observed_at DESC",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(task_run_id, admission, reason, workload_name, transitions)| {
                    KueueWorkloadAdmissionRecord {
                        task_run_id,
                        admission,
                        reason,
                        workload_name,
                        transitions,
                    }
                },
            )
            .collect())
    }
}
