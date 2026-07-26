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
//!
//! # The `task_dispatch` population had no proof at all
//!
//! Migration 153 joined layer-1 dispatch admission to this ledger as the
//! `task_dispatch` consumer kind, and #2608 made it the ONLY authority that
//! decides whether a task-run may spawn. [`lease_object_name`] was never taught
//! about it, so every `task_dispatch` row fell into the unrecognised arm,
//! returned `None`, and was skipped by this sweep — permanently unreclaimable.
//!
//! That is a total dispatch outage rather than a slow warm base. Production on
//! 2026-07-25: three occupying `task_dispatch` rows against `cap 3`, ZERO
//! task-run Pods and ZERO Jobs in the namespace, `build admission denied;
//! leaving task queued  occupancy=3 cap=3` for every task for ~40 minutes. The
//! rows survived a full `djinn-server` rollout restart, that restart's own
//! journal recovery, the startup Kubernetes inventory reconciliation (which
//! reported `stale_rows=2 reclaimed=2` — a DIFFERENT ledger, the v0 lifecycle
//! journal), and the deletion of every `Complete` task-run Job.
//!
//! A dispatch lease cannot be probed the way a warm lease is: its identity is
//! `dispatch:{task_id}:{generation}`, and the Kubernetes Job it leads to is
//! named from a `task_run_id` that does not exist yet at acquisition time.
//! There is no object name to ask about, and guessing one would make the
//! absence proof vacuous. Its owner is instead a durable fact:
//!
//! * a dispatch slot is acquired for exactly one admission generation, and
//! * `BuildAdmissionController::transition_permit` hands that slot back on the
//!   same edge that marks the generation terminal.
//!
//! So a settled dispatch lease whose journal generation is already `terminal`
//! is holding capacity for a lifecycle that provably ended, and the ways that
//! generation reaches `terminal` are exactly the ways the slot should already
//! have been released: the ordinary lifecycle callback, restart recovery
//! (`recover_all_predecessors`, which retires every predecessor-epoch row and
//! releases no lease), and the inventory reconciler's own Kubernetes absence
//! proof. Reclamation therefore INHERITS the journal's evidence instead of
//! inventing a weaker one of its own.
//!
//! What is deliberately NOT proof: a dispatch lease with no journal row at all.
//! `BuildAdmissionController::admit` writes no ledger row while v0 is `off`, so
//! an absent row is genuinely unknown state and stays occupying. The
//! grant-without-a-row window is closed on the other side instead — `admit`
//! hands the slot back when the ledger append fails.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use djinn_db::{
    AdmissionDomain, AdmissionJournalRepository, AdmissionState, BuildLeaseConsumerKind,
    BuildLeaseRepository, BuildLeaseRow, BuildLeaseState, ReclaimAbsentBuildLeaseInput,
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
    /// Leases whose object was proven absent, plus the `task_dispatch` leases
    /// whose owning admission generation is proven terminal. Both are the
    /// "this lease has no owner" population; see `ownerless_dispatch` for the
    /// dispatch share on its own.
    pub absent: usize,
    /// `task_dispatch` leases retired because their owning admission
    /// generation is terminal. Reported separately because this is the
    /// population that produced a total dispatch outage while every
    /// Kubernetes-shaped reconciler reported success.
    pub ownerless_dispatch: usize,
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
/// lease is never reclaimed on a Kubernetes proof: guessing a name is how an
/// absence proof becomes vacuous.
///
/// `task_dispatch` deliberately yields `None`. It is not an oversight and not a
/// name this function could learn: the slot is acquired BEFORE the task-run
/// exists, so the Job name it leads to (`taskrun_job_name(task_run_id)`) is not
/// yet determined. That population is proven ownerless through its admission
/// generation instead — see [`dispatch_identity`] and the module docs.
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

/// The admission generation a `task_dispatch` lease bought capacity for.
///
/// Read from the durable `dispatch:{task_id}:{generation}` identity written by
/// `BuildLeaseService::identity`, never from a process-local counter: the
/// generation a dispatch attempt reserves is resolved by the journal
/// (`resolve_dispatch_generation`) before the slot is acquired, so this is the
/// exact journal key that owns the lease. An identity that does not parse
/// yields `None` and the lease is left occupying.
pub fn dispatch_identity(row: &BuildLeaseRow) -> Option<(String, i64)> {
    if row.key.consumer_kind != BuildLeaseConsumerKind::TaskDispatch {
        return None;
    }
    let rest = row.immutable_identity.strip_prefix("dispatch:")?;
    let (task_id, generation) = rest.rsplit_once(':')?;
    let generation: i64 = generation.parse().ok()?;
    (!task_id.is_empty() && generation >= 0).then(|| (task_id.to_owned(), generation))
}

/// Why one settled occupying lease may be retired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerlessProof {
    /// The authoritative LIST and an independent GET both say the object this
    /// lease committed to does not exist.
    ObjectAbsent,
    /// The lease is a `task_dispatch` row whose admission generation is
    /// terminal: the lifecycle it bought capacity for has ended.
    GenerationTerminal,
    /// The lease is a `task_dispatch` row the FIFO granted to a requester that
    /// had already given up: still `granted`, never acknowledged, and no
    /// admission ever proceeded on it.
    GrantNeverClaimed,
}

impl OwnerlessProof {
    /// Whether this proof is about the `task_dispatch` population, which no
    /// Kubernetes-shaped reclaimer could see at all before #2608's capacity
    /// cut-over was given a sweep of its own.
    fn is_dispatch(self) -> bool {
        matches!(self, Self::GenerationTerminal | Self::GrantNeverClaimed)
    }
}

/// Periodic sweep that retires occupying build leases with no owner.
pub struct BuildLeaseReclaimer {
    repository: Arc<BuildLeaseRepository>,
    /// The v0 lifecycle ledger. Required, not optional: it is the only thing
    /// that can answer who owns a `task_dispatch` lease, and an optional
    /// dependency here would let composition silently reinstate the
    /// unreclaimable population this module exists to retire.
    journal: Arc<AdmissionJournalRepository>,
    inventory: Arc<dyn WorkloadInventory>,
    settle_window: Duration,
    serial: Mutex<()>,
}

impl BuildLeaseReclaimer {
    #[must_use]
    pub fn new(
        repository: Arc<BuildLeaseRepository>,
        journal: Arc<AdmissionJournalRepository>,
        inventory: Arc<dyn WorkloadInventory>,
    ) -> Self {
        Self::with_settle_window(
            repository,
            journal,
            inventory,
            DEFAULT_LEASE_RECLAIM_SETTLE_WINDOW,
        )
    }

    /// Reclaim with an explicit settle window. Tests use a zero window to
    /// exercise reclamation without sleeping; production uses the default.
    #[must_use]
    pub fn with_settle_window(
        repository: Arc<BuildLeaseRepository>,
        journal: Arc<AdmissionJournalRepository>,
        inventory: Arc<dyn WorkloadInventory>,
        settle_window: Duration,
    ) -> Self {
        Self {
            repository,
            journal,
            inventory,
            settle_window,
            serial: Mutex::new(()),
        }
    }

    /// Whether one settled occupying lease is provably ownerless, and why.
    ///
    /// Every branch is a proof, never an age heuristic. `None` means the
    /// question could not be answered — an unrecognised identity, a lease whose
    /// object is still listed, an `Uncertain` probe against a degraded API
    /// server, an unreadable journal, or a dispatch generation that is still
    /// occupying — and an unanswered question always leaves the lease holding
    /// its slot.
    async fn ownerless_proof(
        &self,
        row: &BuildLeaseRow,
        listed_names: &HashSet<String>,
    ) -> Option<OwnerlessProof> {
        if let Some((task_id, generation)) = dispatch_identity(row) {
            return match self
                .journal
                .generation_state(AdmissionDomain::TaskObservation, &task_id, generation)
                .await
            {
                Ok(Some(AdmissionState::Terminal)) => Some(OwnerlessProof::GenerationTerminal),
                // Still occupying the lifecycle ledger: the task-run may be
                // live, and this lease is its capacity.
                Ok(Some(_)) => None,
                // No ledger row at all. That is unknown state, not proof — v0
                // `off` writes no rows — EXCEPT in the one durable state that
                // says the grant was never claimed.
                //
                // `acquire_dispatch_slot` acknowledges every grant it takes
                // (`queue` then `grant`), so a lease a live dispatch holds is
                // always `launching` or later. A settled `granted` row is
                // therefore always a FIFO grant handed to a requester that had
                // already walked away on a `Queued` answer — the same trap the
                // warm path hit — and with no ledger row behind it, no
                // admission ever proceeded on it either. Three independent
                // durable facts, none of them "the row looks old".
                Ok(None) if row.state == BuildLeaseState::Granted => {
                    Some(OwnerlessProof::GrantNeverClaimed)
                }
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(
                        consumer_id = %row.key.consumer_id,
                        %error,
                        "build_lease: admission ledger unreadable; dispatch lease left occupying"
                    );
                    None
                }
            };
        }
        let object_name = lease_object_name(row)?;
        if listed_names.contains(&object_name) {
            return None;
        }
        (self
            .inventory
            .presence(WorkloadObjectKind::Job, &object_name)
            .await
            == ObjectPresence::Absent)
            .then_some(OwnerlessProof::ObjectAbsent)
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
            let Some(proof) = self.ownerless_proof(&row, &listed_names).await else {
                continue;
            };
            report.absent += 1;
            if proof.is_dispatch() {
                report.ownerless_dispatch += 1;
            }
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
                        identity = %row.immutable_identity,
                        proof = ?proof,
                        "build_lease: retired an occupying lease that provably has no owner"
                    );
                }
                Ok(ReclaimAbsentBuildLeaseOutcome::AlreadyTerminal(_)) => {}
                Ok(ReclaimAbsentBuildLeaseOutcome::Fenced { reason }) => {
                    report.fenced += 1;
                    tracing::warn!(
                        consumer_kind = ?row.key.consumer_kind,
                        consumer_id = %row.key.consumer_id,
                        identity = %row.immutable_identity,
                        %reason,
                        "build_lease: refused to retire a lease that changed after its ownerless \
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
