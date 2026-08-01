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
//!   `granted` to the recoverable `suspect` — had no production caller at all
//!   until 2026-07-30 (see "The `queued` row nothing could see" below), and
//!   even now it only exchanges one occupying state for another.
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
//! named from a `task_run_id` that did not exist yet at acquisition time. There
//! is no object name to ask about, and guessing one would make the absence
//! proof vacuous.
//!
//! # Kueue cutover (o53p): the dispatch proof got STRONGER, not weaker
//!
//! This population used to be proven ownerless by reading the v0 lifecycle
//! journal (`admission_journal`): a settled dispatch lease whose admission
//! generation was already `terminal` was holding capacity for a lifecycle that
//! provably ended. That journal was the pre-create reservation authority Kueue
//! replaces, and it is deleted.
//!
//! What replaces it is not a heuristic, it is a stronger fact. NOTHING acquires
//! a `task_dispatch` lease any more: `LeaseIdentity::TaskDispatch` has no
//! constructor anywhere in the workspace, because the pre-create dispatch
//! reservation it served was stood down by the Kueue cutover. So every
//! `task_dispatch` row in `build_leases` is a legacy row whose owner cannot
//! exist, and a settled occupying one is ownerless by construction rather than
//! by inference from a second ledger.
//!
//! That claim is load-bearing, so it is asserted rather than merely stated:
//! `no_dispatch_lease_can_ever_be_acquired_again` fails if a
//! `LeaseIdentity::TaskDispatch` constructor is reintroduced. If one ever is,
//! this proof must be replaced BEFORE that lands, or reclamation would start
//! retiring live dispatch leases.
//!
//! Retiring the legacy rows is the point. Leaving them would strand the
//! cutover's own leftovers against a production reference cap of 3, which is
//! the outage shape this module was written to end.
//!
//! # Presence is not liveness (2026-07-30, v0.7.31)
//!
//! Everything above proves a lease ownerless by showing its object is GONE.
//! That is a strictly weaker proof than it reads as, and the gap is an hour
//! wide.
//!
//! Production, single-node k3s, dispatch paused, every task-run Job at
//! `Complete` (`succeeded=1`, conditions `Suspended → SuccessCriteriaMet →
//! Complete`, zero Pods): three `task_invocation` rows sat in `launching`, plus
//! one in `queued`. They survived ~7 minutes of polling, a full `djinn-server`
//! rollout restart, and every owning Job reaching a terminal condition. Both
//! incidents that day were ended by a hand-written SQL `UPDATE`.
//!
//! The mechanism is that a finished Job does not disappear. Task-run Jobs carry
//! `ttlSecondsAfterFinished: 3600`, so for a full hour after the work ends the
//! object is still listed — and this sweep collapsed the authoritative LIST to
//! a set of NAMES (`records.into_iter().map(|record| record.name)`), discarding
//! the `terminal` flag `job_record` had already computed for every one of them.
//! A listed name short-circuited to "still live". Meanwhile no other path can
//! retire the row either: `release`/`cancel` need the dead worker's fencing
//! token, and `abandon_queued` refuses anything past `queued`.
//!
//! That also explains the one row that DID clear earlier the same day, which is
//! the most informative fact in the report: its Job's TTL had already fired, so
//! the object was genuinely absent and the old proof applied. The difference
//! between the row that healed and the four that did not was never the lease —
//! it was whether Kubernetes had gotten around to deleting the Job.
//!
//! So the LIST now keeps the terminal flag, and a listed-but-terminal object is
//! [`OwnerlessProof::ObjectFinished`]. This is still observable state, never
//! elapsed time: a Job with no terminal condition is live work at any age, and
//! a degraded API server still yields no proof at all.
//!
//! # The `queued` row nothing could see
//!
//! The fourth stranded row was `queued`, and it was unreachable by every reaper
//! in the process for a different reason: `queued` holds no capacity, so it is
//! not in `OCCUPYING`, so the sweep's own listing filtered it out. The only
//! thing that retires a queued row is `expire_queued_tx`, which runs INSIDE
//! `grant_next` — i.e. only when some other lease is being drained. With
//! dispatch paused, nothing calls `grant_next`, and the row is immortal.
//! `BuildLeaseRepository::expire_deadlines`, the one method that sweeps queue
//! deadlines on their own, had no production caller anywhere in the workspace.
//!
//! `deploy/kueue/preflight.sh` fences the authority cutover on `state <>
//! 'terminal'`, which counts `queued`, so that row blocked the cutover
//! indefinitely while every occupancy-shaped reconciler reported clean.
//!
//! The sweep is therefore over NONTERMINAL rows, and the periodic loop that
//! drives it now also calls `expire_deadlines`. Widening the sweep to `queued`
//! makes the absence proof vacuous for one population — a `graph_warm` lease
//! POSTs its Job only AFTER it is granted, so a queued warm lease has no object
//! and never did — which [`object_predates_lease`] excludes explicitly.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseRepository, BuildLeaseRow, BuildLeaseState,
    BuildLeaseTerminalReason, ReclaimAbsentBuildLeaseInput, ReclaimAbsentBuildLeaseOutcome,
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
    /// Nonterminal leases the pass examined. Named for what it counts: the
    /// sweep covers `queued` as well as the five occupying states, because
    /// `queued` was the one population no reaper could see.
    pub examined: usize,
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
    /// Leases retired because their Kubernetes object still EXISTS but has
    /// reached a terminal condition. Reported separately because this is the
    /// population no absence proof could ever see, and the one that stranded
    /// three `launching` rows behind three `Complete` Jobs in production.
    pub finished_object: usize,
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
/// name this function could learn: the slot was acquired BEFORE the task-run
/// existed, so the Job name it leads to (`taskrun_job_name(task_run_id)`) was
/// never determined. That population is proven ownerless by the absence of any
/// possible acquirer instead — see the module docs.
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

/// Why one settled nonterminal lease may be retired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerlessProof {
    /// The authoritative LIST and an independent GET both say the object this
    /// lease committed to does not exist.
    ObjectAbsent,
    /// The object EXISTS and has reached a terminal condition (`Complete` or
    /// `Failed`). Nothing will run under it again, so the holder that would
    /// have released this lease is gone — and the object's continued existence
    /// for its `ttlSecondsAfterFinished` is not evidence to the contrary.
    ObjectFinished,
    /// The lease is a `task_dispatch` row, and the pre-create dispatch
    /// reservation that was its only possible acquirer no longer exists (Kueue
    /// cutover, o53p). No owner can exist, so this is legacy capacity to
    /// retire.
    DispatchAuthorityDeleted,
}

impl OwnerlessProof {
    /// Whether this proof is about the `task_dispatch` population, which no
    /// Kubernetes-shaped reclaimer could see at all before #2608's capacity
    /// cut-over was given a sweep of its own.
    fn is_dispatch(self) -> bool {
        matches!(self, Self::DispatchAuthorityDeleted)
    }

    /// The reason stamped on the retired row, so the ledger records WHICH proof
    /// ended the lease rather than flattening every reclamation into one word.
    fn terminal_reason(self) -> BuildLeaseTerminalReason {
        match self {
            Self::ObjectAbsent | Self::DispatchAuthorityDeleted => {
                BuildLeaseTerminalReason::ReclaimedAbsent
            }
            Self::ObjectFinished => BuildLeaseTerminalReason::ReclaimedFinished,
        }
    }
}

/// Whether this lease's Kubernetes object is guaranteed to ALREADY EXIST, so
/// that "no such object" is evidence of a dead owner rather than of an object
/// that was never going to exist yet.
///
/// This is the guard that keeps the absence proof from becoming vacuous now
/// that the sweep also sees `queued` rows:
///
/// * a `task_invocation` lease is asked for from INSIDE a running task-run pod,
///   so its Job predates the lease in every nonterminal state — absence is real
///   evidence even while the lease is still queued;
/// * a `graph_warm` lease is what AUTHORIZES the warm Job POST, so a `queued`
///   warm lease has no object yet and never did. Probing for it would find
///   nothing and retire a legitimately queued warm request — one that may sit
///   untouched behind a full cap for hours, so no settle window saves it.
///   Only its queue deadline may retire it.
fn object_predates_lease(row: &BuildLeaseRow) -> bool {
    match row.key.consumer_kind {
        BuildLeaseConsumerKind::TaskInvocation => true,
        BuildLeaseConsumerKind::GraphWarm | BuildLeaseConsumerKind::TaskDispatch => {
            row.state != BuildLeaseState::Queued
        }
    }
}

/// Periodic sweep that retires occupying build leases with no owner.
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

    /// Whether one settled occupying lease is provably ownerless, and why.
    ///
    /// Every branch is a proof, never an age heuristic. `None` means the
    /// question could not be answered — an unrecognised identity, a lease whose
    /// object is still listed, or an `Uncertain` probe against a degraded API
    /// server — and an unanswered question always leaves the lease holding its
    /// slot.
    async fn ownerless_proof(
        &self,
        row: &BuildLeaseRow,
        listed: &HashMap<String, bool>,
    ) -> Option<OwnerlessProof> {
        if row.key.consumer_kind == BuildLeaseConsumerKind::TaskDispatch {
            // See the module docs. Nothing constructs `LeaseIdentity::TaskDispatch`
            // any more, so a settled occupying dispatch lease has no possible
            // owner. This is a durable fact about the code, not an age heuristic,
            // and `no_dispatch_lease_can_ever_be_acquired_again` fails if it stops
            // being true.
            return Some(OwnerlessProof::DispatchAuthorityDeleted);
        }
        if !object_predates_lease(row) {
            return None;
        }
        let object_name = lease_object_name(row)?;
        if let Some(&terminal) = listed.get(&object_name) {
            // The object is here. Whether that means its owner is ALIVE is the
            // entire question, and for an hour after a task-run Job completes
            // the answer is no: the Job lingers for its `ttlSecondsAfterFinished`
            // while the worker that would have sent `release_lease` is gone.
            // Reading presence as liveness is what stranded three `launching`
            // rows behind three `Complete` Jobs in production.
            return terminal.then_some(OwnerlessProof::ObjectFinished);
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
        //
        // Each listed object is kept WITH its terminal flag. Collapsing this to
        // a set of names — which is what it used to be — throws away the only
        // evidence that separates "this Job is running" from "this Job finished
        // and is waiting out its TTL", and the second is exactly when a lease
        // needs retiring.
        let listed: HashMap<String, bool> = match self.inventory.list().await {
            Ok(records) => records
                .into_iter()
                .map(|record| (record.name, record.terminal))
                .collect(),
            Err(error) => {
                report.blockers.push(error);
                return report;
            }
        };

        let rows = match self
            .repository
            .list_nonterminal_with_settlement(self.settle_window.as_secs() as i64)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                report.blockers.push(error.to_string());
                return report;
            }
        };
        report.examined = rows.len();

        for (row, settled) in rows {
            let lease = format!("{:?}/{}", row.key.consumer_kind, row.key.consumer_id);
            if !settled {
                continue;
            }
            let Some(proof) = self.ownerless_proof(&row, &listed).await else {
                continue;
            };
            report.absent += 1;
            if proof.is_dispatch() {
                report.ownerless_dispatch += 1;
            }
            if proof == OwnerlessProof::ObjectFinished {
                report.finished_object += 1;
            }
            let input = ReclaimAbsentBuildLeaseInput {
                key: row.key.clone(),
                observed_state: row.state,
                observed_immutable_identity: row.immutable_identity.clone(),
                observed_fencing_token: row.fencing_token,
                observed_bound_pod_uid: row.bound_pod_uid.clone(),
                observed_updated_at: row.updated_at.clone(),
                terminal_reason: proof.terminal_reason(),
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
