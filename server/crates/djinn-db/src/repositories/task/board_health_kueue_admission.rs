//! The dispatch-gate reader for the Kueue admission projection (migration 165).
//!
//! # Why this exists
//!
//! `kueue_workload_admission` is written by `djinn-server`'s
//! `kueue_workload_reconcile` Workload reflector and, until this module, was
//! read by nothing. A written-but-unread projection is not a source of truth:
//! it is a table that happens to have rows in it. Worse, the Kueue cutover moved
//! build capacity onto a ClusterQueue, so the one question the board most needs
//! answered once Kueue is armed — *is this board sitting still because Kueue has
//! not admitted anything?* — had no durable answer anywhere. On 2026-07-30 the
//! absence of exactly that surface turned a pod-permit wedge into a ninety
//! minute diagnosis.
//!
//! # The distinction this module refuses to collapse
//!
//! Production today runs `kueue.armed=false`: no namespace is Kueue-managed, no
//! Job is suspended, and there are **zero** Workloads. Reporting that as
//! "pending" would describe a healthy unarmed cluster as a stalled one, which is
//! strictly worse than reporting nothing — the operator who lost ninety minutes
//! did so following a health surface that was confidently wrong, not one that
//! was silent.
//!
//! So the projection has three outcomes here and they are never merged:
//!
//! * [`KueueProjection::Unobservable`] — the relation could not be read. Never
//!   substituted with "no rows"; that substitution is the mistake
//!   [`super::board_health_dispatch_gate`] exists to undo.
//! * [`KueueProjection::Inert`] — the relation was read and holds **no rows at
//!   all**. The reflector only runs when Kueue is armed, on the leader, against
//!   a Kubernetes runtime, so an empty relation means no build Workload has been
//!   observed by this deployment. Nothing is queued behind Kueue; this is the
//!   shipped default and it is NOT a stall.
//! * [`KueueProjection::Observing`] — the relation holds rows, so the reflector
//!   has observed at least one Workload and Kueue is in the loop.
//!
//! `kueue_clusterqueue_admission` moves from `unevaluated_gates` to
//! `evaluated_gates` **only** in the `Observing` case. Under `Inert` a dead
//! reflector (leader failover, no Kubernetes client) is indistinguishable from
//! an unarmed cluster from a Postgres read, and claiming to have evaluated a
//! gate whose evidence source might simply not be running is precisely the
//! "confidently wrong" failure this section was rewritten to remove.
//!
//! # Why a pending row usually has no task
//!
//! The projection is keyed by `task_runs.id`, but under create-then-admit the
//! `task_runs` row is created by the **in-pod supervisor** — which does not run
//! until Kueue admits the Job. So a genuinely-pending Workload structurally has
//! no `task_runs` row and therefore no task to attribute it to. That is not a
//! defect of this reader; it is the window the projection exists to make
//! visible. Such rows are surfaced in `pending_task_runs` with an explicit
//! `task_id: null`, never attached to a task.
//!
//! The consequence for a row whose task-run does not exist — whether it never
//! did or was deleted underneath it — is that it can never become a phantom
//! per-task entry: `by_task` is built from an INNER JOIN against `task_runs`, so
//! an unattributable row contributes to the counts and to `pending_task_runs`
//! (with a null task) and to nothing else.

use std::collections::HashMap;

use sqlx::Row;

/// The dispatcher gate this reader answers for, named identically to the entry
/// [`super::board_health_dispatch_gate::UNEVALUATED_GATES`] used to carry
/// unconditionally.
pub(super) const KUEUE_GATE: &str = "kueue_clusterqueue_admission";

/// How many pending entries travel in the payload.
///
/// The block is emitted once per stranded finding, so an unbounded list would
/// multiply. Ten is enough to name the head of a queue; the counts above it are
/// exact regardless.
const MAX_PENDING_ENTRIES: i64 = 10;

/// Pool-wide counts over the whole projection.
#[derive(Clone, Debug)]
pub(super) struct KueueSummary {
    pub total: i64,
    pub pending: i64,
    pub admitted: i64,
    pub finished: i64,
    /// Rows with no `task_runs` row. Under create-then-admit this is the normal
    /// state of every genuinely-pending Workload, not an anomaly.
    pub without_task_run: i64,
    /// Age of the STALEST `observed_at` in the relation.
    ///
    /// A Kubernetes watch resync refreshes `observed_at` on every row it
    /// replays, so this is small while the reflector is alive and grows without
    /// bound once it stops. It is the only signal available from Postgres that
    /// distinguishes "Kueue is watching and nothing changed" from "nobody has
    /// looked at Kueue since the last leader died".
    pub stalest_observation_age_seconds: Option<i64>,
    /// Age of the oldest `first_seen_at` among PENDING rows: how long the
    /// longest-waiting Workload has been known about at all.
    pub oldest_pending_first_seen_age_seconds: Option<i64>,
}

/// One projection row, with its task attribution when one exists.
#[derive(Clone, Debug)]
pub(super) struct KueueAdmissionRow {
    pub task_run_id: String,
    /// `pending`, `admitted` or `finished`.
    pub admission: String,
    /// Kueue's own word for the state, when it gave one.
    pub reason: Option<String>,
    pub workload_name: Option<String>,
    /// Observed state CHANGES, not observations — a watch resync does not move
    /// this. A high count on a `pending` row is quota thrash.
    pub transitions: i64,
    /// `null` when no `task_runs` row exists for `task_run_id`.
    pub task_id: Option<String>,
    pub task_short_id: Option<String>,
    pub first_seen_at: Option<String>,
    pub observed_at: Option<String>,
    pub first_seen_age_seconds: i64,
    pub observed_age_seconds: i64,
}

impl KueueAdmissionRow {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "task_run_id":            self.task_run_id,
            "admission":              self.admission,
            "reason":                 self.reason,
            "workload_name":          self.workload_name,
            "transitions":            self.transitions,
            "task_id":                self.task_id,
            "task_short_id":          self.task_short_id,
            "first_seen_at":          self.first_seen_at,
            "observed_at":            self.observed_at,
            "first_seen_age_seconds": self.first_seen_age_seconds,
            "observed_age_seconds":   self.observed_age_seconds,
        })
    }
}

/// What this section managed to learn about `kueue_workload_admission`.
#[derive(Clone, Debug)]
pub(super) enum KueueProjection {
    /// The relation could not be read. Deliberately not "no rows".
    Unobservable { detail: &'static str },
    /// Read, and empty. No Workload has ever been observed by this deployment.
    Inert,
    /// Read, and non-empty: the reflector has seen Kueue decide something.
    Observing {
        summary: KueueSummary,
        /// The head of the pending queue, oldest first. Includes rows with no
        /// task attribution, which is what a genuinely-pending Workload looks
        /// like.
        pending: Vec<KueueAdmissionRow>,
        /// Newest projection row per task, for rows that join to a live
        /// `task_runs` row. An unattributable row is absent by construction.
        by_task: HashMap<String, KueueAdmissionRow>,
    },
}

/// Read the projection once for the whole board-health section.
///
/// The empty case costs exactly one aggregate query — which is the case
/// production is in today — and only a non-empty relation pays for the detail
/// queries.
pub(super) async fn load_kueue_projection(pool: &sqlx::PgPool) -> KueueProjection {
    let summary_sql = r"SELECT
             COUNT(*)::BIGINT AS total,
             COUNT(*) FILTER (WHERE k.admission = 'pending')::BIGINT   AS pending,
             COUNT(*) FILTER (WHERE k.admission = 'admitted')::BIGINT  AS admitted,
             COUNT(*) FILTER (WHERE k.admission = 'finished')::BIGINT  AS finished,
             COUNT(*) FILTER (
                 WHERE NOT EXISTS (SELECT 1 FROM task_runs r WHERE r.id = k.task_run_id)
             )::BIGINT AS without_task_run,
             FLOOR(EXTRACT(EPOCH FROM (now() - MIN(k.observed_at))))::BIGINT
                 AS stalest_observation_age_seconds,
             FLOOR(EXTRACT(EPOCH FROM (
                 now() - MIN(k.first_seen_at) FILTER (WHERE k.admission = 'pending')
             )))::BIGINT AS oldest_pending_first_seen_age_seconds
           FROM kueue_workload_admission k";

    let Ok(summary_row) = sqlx::query(summary_sql).fetch_one(pool).await else {
        return KueueProjection::Unobservable {
            detail: "kueue_workload_admission could not be read",
        };
    };

    let summary = KueueSummary {
        total: summary_row.try_get("total").unwrap_or(0),
        pending: summary_row.try_get("pending").unwrap_or(0),
        admitted: summary_row.try_get("admitted").unwrap_or(0),
        finished: summary_row.try_get("finished").unwrap_or(0),
        without_task_run: summary_row.try_get("without_task_run").unwrap_or(0),
        stalest_observation_age_seconds: summary_row
            .try_get("stalest_observation_age_seconds")
            .ok()
            .flatten(),
        oldest_pending_first_seen_age_seconds: summary_row
            .try_get("oldest_pending_first_seen_age_seconds")
            .ok()
            .flatten(),
    };

    if summary.total == 0 {
        return KueueProjection::Inert;
    }

    let Some(pending) = load_pending(pool).await else {
        return KueueProjection::Unobservable {
            detail: "kueue_workload_admission pending rows could not be read",
        };
    };
    let Some(by_task) = load_by_task(pool).await else {
        return KueueProjection::Unobservable {
            detail: "kueue_workload_admission could not be joined to task_runs",
        };
    };

    KueueProjection::Observing {
        summary,
        pending,
        by_task,
    }
}

/// Common projection columns, with the task attribution LEFT-joined on.
const ROW_COLUMNS: &str = r#"k.task_run_id,
        k.admission,
        k.reason,
        k.workload_name,
        k.transitions::BIGINT AS transitions,
        r.task_id,
        t.short_id AS task_short_id,
        to_char(k.first_seen_at AT TIME ZONE 'utc',
                'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS first_seen_at,
        to_char(k.observed_at AT TIME ZONE 'utc',
                'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"') AS observed_at,
        FLOOR(EXTRACT(EPOCH FROM (now() - k.first_seen_at)))::BIGINT AS first_seen_age_seconds,
        FLOOR(EXTRACT(EPOCH FROM (now() - k.observed_at)))::BIGINT   AS observed_age_seconds"#;

fn row_from(row: &sqlx::postgres::PgRow) -> Option<KueueAdmissionRow> {
    Some(KueueAdmissionRow {
        task_run_id: row.try_get("task_run_id").ok()?,
        admission: row.try_get("admission").ok()?,
        reason: row.try_get("reason").ok().flatten(),
        workload_name: row.try_get("workload_name").ok().flatten(),
        transitions: row.try_get("transitions").unwrap_or(0),
        task_id: row.try_get("task_id").ok().flatten(),
        task_short_id: row.try_get("task_short_id").ok().flatten(),
        first_seen_at: row.try_get("first_seen_at").ok().flatten(),
        observed_at: row.try_get("observed_at").ok().flatten(),
        first_seen_age_seconds: row.try_get("first_seen_age_seconds").unwrap_or(0),
        observed_age_seconds: row.try_get("observed_age_seconds").unwrap_or(0),
    })
}

/// The head of the pending queue, oldest-known first.
///
/// LEFT JOIN, deliberately: a pending Workload whose Job has never been admitted
/// has no `task_runs` row and dropping it here would hide the exact population
/// this projection exists to expose.
async fn load_pending(pool: &sqlx::PgPool) -> Option<Vec<KueueAdmissionRow>> {
    let sql = format!(
        "SELECT {ROW_COLUMNS}
           FROM kueue_workload_admission k
           LEFT JOIN task_runs r ON r.id = k.task_run_id
           LEFT JOIN tasks t     ON t.id = r.task_id
          WHERE k.admission = 'pending'
          ORDER BY k.first_seen_at ASC
          LIMIT {MAX_PENDING_ENTRIES}"
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await.ok()?;
    Some(rows.iter().filter_map(row_from).collect())
}

/// Newest projection row per task.
///
/// INNER JOIN, equally deliberately: this map is what a task's `dispatch_gate`
/// claims about ITS OWN Workload, so a row that cannot be tied to a live
/// `task_runs` row must not reach it. That is the whole phantom-entry
/// protection — an orphaned projection row is counted and listed, and attributed
/// to nobody.
async fn load_by_task(pool: &sqlx::PgPool) -> Option<HashMap<String, KueueAdmissionRow>> {
    let sql = format!(
        "SELECT DISTINCT ON (r.task_id) {ROW_COLUMNS}
           FROM kueue_workload_admission k
           JOIN task_runs r ON r.id = k.task_run_id
           LEFT JOIN tasks t ON t.id = r.task_id
          ORDER BY r.task_id, k.observed_at DESC"
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await.ok()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                let parsed = row_from(row)?;
                let task_id = parsed.task_id.clone()?;
                Some((task_id, parsed))
            })
            .collect(),
    )
}

/// Result of applying the Kueue admission gate to one task.
pub(super) struct KueueGateOutcome {
    /// Pool-wide projection state as JSON, or `null` when it was unreadable.
    pub kueue_admission: serde_json::Value,
    /// This task's own projection row as JSON, or `null` when it has none.
    pub kueue_workload: serde_json::Value,
    /// Machine-readable reasons contributed by this gate.
    pub reasons: Vec<&'static str>,
    /// True only under [`KueueProjection::Observing`]. See the module docs for
    /// why an empty relation does NOT count as an evaluation.
    pub evaluated: bool,
    /// Why the gate could not be evaluated, surfaced in `coverage`.
    pub unevaluated_detail: Option<&'static str>,
}

/// Apply the Kueue admission gate to one task.
pub(super) fn kueue_gate(projection: &KueueProjection, task_id: &str) -> KueueGateOutcome {
    match projection {
        KueueProjection::Unobservable { detail } => KueueGateOutcome {
            kueue_admission: serde_json::Value::Null,
            kueue_workload: serde_json::Value::Null,
            reasons: Vec::new(),
            evaluated: false,
            unevaluated_detail: Some(detail),
        },
        KueueProjection::Inert => KueueGateOutcome {
            kueue_admission: serde_json::json!({
                "authority":        "kueue_workload_admission",
                "projection_state": "no_workloads_observed",
                "total":            0,
                "pending":          0,
                "admitted":         0,
                "finished":         0,
                "without_task_run": 0,
                "stalest_observation_age_seconds": serde_json::Value::Null,
                "oldest_pending_first_seen_age_seconds": serde_json::Value::Null,
                "pending_task_runs": serde_json::Value::Array(Vec::new()),
                "note": "The Kueue admission projection (migration 165) holds NO rows. Its \
                         only writer is the leader's Workload reflector, which does not start \
                         unless Kueue is armed against a Kubernetes runtime, so an empty \
                         relation means this deployment has observed no build Workload at \
                         all. That is the shipped default (`kueue.armed=false`) and it is \
                         explicitly NOT a stalled queue: nothing is pending. It is also not \
                         proof that Kueue admitted anything — a reflector that never started, \
                         or stopped, looks identical from here, which is why \
                         `kueue_clusterqueue_admission` stays in `unevaluated_gates` in this \
                         state.",
            }),
            kueue_workload: serde_json::Value::Null,
            reasons: Vec::new(),
            evaluated: false,
            unevaluated_detail: Some(
                "kueue_workload_admission holds no rows: Kueue is unarmed, or armed and has \
                 observed no Workload, and the two are indistinguishable from Postgres",
            ),
        },
        KueueProjection::Observing {
            summary,
            pending,
            by_task,
        } => {
            let mine = by_task.get(task_id);
            let mut reasons: Vec<&'static str> = Vec::new();
            if let Some(row) = mine {
                match row.admission.as_str() {
                    // Kueue has this task's Workload queued: the Job is
                    // suspended behind ClusterQueue quota. This is the reason
                    // that had no durable source at all before migration 165.
                    "pending" => reasons.push("kueue_workload_pending"),
                    // The Workload was admitted but the task is still stranded
                    // with no running session, so the run behind it is gone.
                    // Reported, because a projection that says "admitted" for a
                    // task nothing is running is the tombstone shape of #2661.
                    "admitted" => reasons.push("kueue_workload_admitted_without_session"),
                    _ => {}
                }
            }

            let kueue_admission = serde_json::json!({
                "authority":        "kueue_workload_admission",
                "projection_state": "observing",
                "total":            summary.total,
                "pending":          summary.pending,
                "admitted":         summary.admitted,
                "finished":         summary.finished,
                "without_task_run": summary.without_task_run,
                "stalest_observation_age_seconds": summary.stalest_observation_age_seconds,
                "oldest_pending_first_seen_age_seconds":
                    summary.oldest_pending_first_seen_age_seconds,
                "pending_entry_limit": MAX_PENDING_ENTRIES,
                "pending_task_runs": pending
                    .iter()
                    .map(KueueAdmissionRow::to_json)
                    .collect::<Vec<_>>(),
                "note": "Kueue's OWN admission decision as the leader's Workload reflector \
                         observed it (migration 165). `pending` is the number of build \
                         Workloads Kueue has NOT admitted — Jobs sitting suspended behind \
                         ClusterQueue quota, which leave no row in `build_leases` and used to \
                         leave no trace anywhere. `without_task_run` is normal rather than \
                         alarming: under create-then-admit the `task_runs` row is written by \
                         the in-pod supervisor, which cannot run until Kueue admits, so a \
                         genuinely-pending Workload has no task to attribute it to and is \
                         listed with `task_id: null`. `stalest_observation_age_seconds` is \
                         the liveness check on the reflector itself: a watch resync refreshes \
                         every row it replays, so a large value means nobody is watching \
                         Kueue rather than that Kueue is idle.",
            });

            KueueGateOutcome {
                kueue_admission,
                kueue_workload: mine.map_or(serde_json::Value::Null, KueueAdmissionRow::to_json),
                reasons,
                evaluated: true,
                unevaluated_detail: None,
            }
        }
    }
}

#[cfg(test)]
#[path = "board_health_kueue_admission_tests.rs"]
mod tests;
