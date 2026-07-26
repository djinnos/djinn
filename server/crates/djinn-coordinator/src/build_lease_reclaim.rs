//! Reclamation of v1 build leases whose Kubernetes object is provably gone.
//!
//! The v1 lease ledger has no reclaimer of its own. Every exit from an
//! occupying state needs the holder: `release`/`cancel` carry the fencing
//! token, and `abandon_queued` refuses anything past `queued`. Two further
//! gaps close the trap on a grant whose requester gave up on a `Queued` answer
//! before ever acknowledging it — which is how the FIFO ends up granting a slot
//! to nobody, since a later `queue`'s drain always grants the FIFO head:
//!
//! * `BuildLeaseGraphWarmAdapter::recoverable` maps only
//!   `Launching`/`Bound`/`Active`/`Suspect` and returns `None` for everything
//!   else, so a `granted` row is invisible to `reconcile_durable_warm_leases`;
//! * `BuildLeaseService::expire_deadlines` — the one thing that would move
//!   `granted` to the recoverable `suspect` — has no production caller at all,
//!   and even when called it only exchanges one occupying state for another.
//!
//! Neither gap is a deadline problem, which is why this module reads no
//! deadline. (#2605 fixed a real defect one layer over: the shared column list
//! rendered PostgreSQL's timestamp format while `build_lease::ms` parsed
//! RFC3339 and mapped failure to `0` — already the encoding for "no deadline" —
//! so every *recovered* lease came back unbounded. That governs the
//! `launching`-and-later rows `recoverable` can see; it cannot reach a
//! `granted` row, and it cannot make an uncalled `expire_deadlines` run.)
//!
//! That is the production wedge this module closes: three `granted` graph-warm
//! rows against a cap of three, no Kubernetes object behind any of them, every
//! later warm answered `graph warm lease is queued`, and a warm base that had
//! not re-converged for four days while still seeding perfectly.
//!
//! The discipline is the admission journal's (`reclaim_absent_object`), and
//! deliberately not "the row looks old":
//!
//! * the lease has settled — untouched for the whole settle window, so no
//!   in-flight create can still be mid-POST for it;
//! * the authoritative LIST that just succeeded contains no object under this
//!   lease's deterministic name;
//! * a direct GET, taken now and independently of that LIST, answers
//!   [`ObjectPresence::Absent`]. `Uncertain` — a transport or permission
//!   failure — is never proof, so a degraded API server leaves every lease
//!   occupying;
//! * the durable write is a compare-and-set on the full observed identity, so a
//!   holder that acknowledges between the proof and the write fences the
//!   reclamation instead of losing its lease.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseRepository, BuildLeaseRow, ReclaimAbsentBuildLeaseInput,
    ReclaimAbsentBuildLeaseOutcome,
};
use djinn_k8s::{
    ObjectPresence, WorkloadInventory, WorkloadObjectKind, deterministic_warm_job_name,
    taskrun_job_name,
};
use tokio::sync::Mutex;

/// How long an occupying lease must stay untouched before its absence proof is
/// allowed to retire it. Matches the admission journal's reclaim settle window:
/// both windows exist to outlast an in-flight create whose object the API
/// server has not yet made visible.
pub const DEFAULT_LEASE_RECLAIM_SETTLE_WINDOW: Duration = Duration::from_secs(300);

/// Bound on how many individual reclaim failures one pass names. The count is
/// always exact; the samples exist so an operator can see *which* lease without
/// a log line that grows with the stale population.
const MAX_NAMED_FAILURES: usize = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildLeaseReclaimReport {
    /// Occupying leases the pass examined.
    pub occupying: usize,
    /// Leases whose object was proven absent.
    pub absent: usize,
    /// Leases retired by this pass.
    pub reclaimed: usize,
    /// Leases that changed after their absence proof and were left alone.
    pub fenced: usize,
    /// Per-lease failures. Bounded by [`MAX_NAMED_FAILURES`]; see
    /// `failure_count` for the exact total.
    pub failures: Vec<String>,
    /// Exact number of per-lease failures, named or not.
    pub failure_count: usize,
    /// Pass-level failures: an unusable Kubernetes listing or an unreadable
    /// ledger. These mean the pass could not be trusted at all.
    pub blockers: Vec<String>,
}

impl BuildLeaseReclaimReport {
    fn fail(&mut self, lease: &str, error: &str) {
        self.failure_count += 1;
        if self.failures.len() < MAX_NAMED_FAILURES {
            self.failures.push(format!("{lease}: {error}"));
        }
    }
}

/// The Kubernetes object name a lease's immutable identity commits to.
///
/// Derived from the durable identity, never from a process-local attempt id, so
/// the absence probe asks about exactly the object this lease would have
/// created. An identity this function does not recognise yields `None` and the
/// lease is never reclaimed: guessing a name is how an absence proof becomes
/// vacuous.
pub fn lease_object_name(row: &BuildLeaseRow) -> Option<String> {
    let mut fields = row.immutable_identity.split(':');
    match (row.key.consumer_kind, fields.next()?) {
        (BuildLeaseConsumerKind::GraphWarm, "warm") => {
            let project_id = fields.next()?;
            let warm_request_id = fields.next()?;
            (!project_id.is_empty() && !warm_request_id.is_empty())
                .then(|| deterministic_warm_job_name(project_id, warm_request_id))
        }
        (BuildLeaseConsumerKind::TaskInvocation, "task") => {
            let _task_id = fields.next()?;
            let task_run_id = fields.next()?;
            (!task_run_id.is_empty()).then(|| taskrun_job_name(task_run_id))
        }
        _ => None,
    }
}

/// Periodic sweep that retires occupying build leases with no Kubernetes object.
pub struct BuildLeaseReclaimer {
    repository: Arc<BuildLeaseRepository>,
    inventory: Arc<dyn WorkloadInventory>,
    settle_window: Duration,
    serial: Mutex<()>,
}

impl BuildLeaseReclaimer {
    #[must_use]
    pub fn new(
        repository: Arc<BuildLeaseRepository>,
        inventory: Arc<dyn WorkloadInventory>,
    ) -> Self {
        Self::with_settle_window(repository, inventory, DEFAULT_LEASE_RECLAIM_SETTLE_WINDOW)
    }

    /// Reclaim with an explicit settle window. Tests use a zero window to
    /// exercise reclamation without sleeping; production uses the default.
    #[must_use]
    pub fn with_settle_window(
        repository: Arc<BuildLeaseRepository>,
        inventory: Arc<dyn WorkloadInventory>,
        settle_window: Duration,
    ) -> Self {
        Self {
            repository,
            inventory,
            settle_window,
            serial: Mutex::new(()),
        }
    }

    /// One reclamation pass.
    ///
    /// Freed capacity is not granted here: granting is the lease service's
    /// FIFO decision and every `queue` drains it, so the next build request
    /// takes the slot this pass released. Reclamation deliberately owns no
    /// capacity arithmetic.
    pub async fn reclaim(&self) -> BuildLeaseReclaimReport {
        let _serial = self.serial.lock().await;
        let mut report = BuildLeaseReclaimReport::default();

        // An unusable listing is a pass-level blocker: without an authoritative
        // namespace view nothing below is evidence of anything.
        let listed_names: HashSet<String> = match self.inventory.list().await {
            Ok(records) => records.into_iter().map(|record| record.name).collect(),
            Err(error) => {
                report.blockers.push(error);
                return report;
            }
        };

        let rows = match self
            .repository
            .list_occupying_with_settlement(self.settle_window.as_secs() as i64)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                report.blockers.push(error.to_string());
                return report;
            }
        };
        report.occupying = rows.len();

        for (row, settled) in rows {
            let lease = format!("{:?}/{}", row.key.consumer_kind, row.key.consumer_id);
            if !settled {
                continue;
            }
            let Some(object_name) = lease_object_name(&row) else {
                continue;
            };
            if listed_names.contains(&object_name) {
                continue;
            }
            if self
                .inventory
                .presence(WorkloadObjectKind::Job, &object_name)
                .await
                != ObjectPresence::Absent
            {
                continue;
            }
            report.absent += 1;
            let input = ReclaimAbsentBuildLeaseInput {
                key: row.key.clone(),
                observed_state: row.state,
                observed_immutable_identity: row.immutable_identity.clone(),
                observed_fencing_token: row.fencing_token,
                observed_bound_pod_uid: row.bound_pod_uid.clone(),
                observed_updated_at: row.updated_at.clone(),
            };
            match self.repository.reclaim_absent_object(&input).await {
                Ok(ReclaimAbsentBuildLeaseOutcome::Reclaimed(_)) => {
                    report.reclaimed += 1;
                    tracing::warn!(
                        consumer_kind = ?row.key.consumer_kind,
                        consumer_id = %row.key.consumer_id,
                        state = ?row.state,
                        object = %object_name,
                        "build_lease: retired an occupying lease whose Kubernetes object is \
                         provably absent"
                    );
                }
                Ok(ReclaimAbsentBuildLeaseOutcome::AlreadyTerminal(_)) => {}
                Ok(ReclaimAbsentBuildLeaseOutcome::Fenced { reason }) => {
                    report.fenced += 1;
                    tracing::warn!(
                        consumer_kind = ?row.key.consumer_kind,
                        consumer_id = %row.key.consumer_id,
                        object = %object_name,
                        %reason,
                        "build_lease: refused to retire a lease that changed after its absence \
                         proof"
                    );
                }
                Err(error) => report.fail(&lease, &error.to_string()),
            }
        }
        report
    }
}

#[cfg(test)]
#[path = "build_lease_reclaim_tests.rs"]
mod tests;
