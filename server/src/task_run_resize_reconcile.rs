//! The externally owned resize reconciler: a worker's death must not strand
//! resize responsibility inside the Pod that died.
//!
//! # Why this is load-bearing, not defensive
//!
//! The only per-invocation terminal signal the server ever receives is the
//! `release_lease` RPC. It is emitted **only** by a worker that reached
//! `queued == true` and survived long enough to run `reconcile_terminal_lease`.
//! A Pod that is OOM-killed, evicted, or whose node disappears sends nothing at
//! all, and a server that restarts mid-lift loses every in-process handle that
//! could have noticed. In both cases the launcher keeps whatever ceiling the
//! lift left on it, the `build_pod_permits` row never leaves its nonterminal
//! resize state, and the durable `build_leases` row is held by nobody who will
//! ever release it.
//!
//! So the drop cannot be owned by the process that performed the lift. It has
//! to be owned by something that outlives it, reads the durable row, and does
//! not need one line of code in the dead worker to cooperate. That is this
//! module.
//!
//! # What "owned externally" costs, and how it is paid
//!
//! Everything this reconciler knows comes from
//! [`BuildPodPermitRepository::list_nonterminal_resize`] — a durable read. It
//! holds no map of live invocations, and deliberately so: a cache is exactly
//! the state a restart destroys, and seeding resumption from one would make
//! every criterion in this slice pass against a process that never restarted.
//!
//! # The strand predicate, and why a live build is not touched
//!
//! `lifted` is the NORMAL state of a healthy build, and `birth_confirmed` is
//! the resting state of every captured permit between invocations. A reconciler
//! that dropped every nonterminal row it found would take the CPU away from
//! every running build in the cluster on its first tick. So a row is the
//! reconciler's responsibility only when one of two **worker-independent**
//! facts is true:
//!
//! 1. **A drop is already owed.** `drop_required`, `drop_applying` and
//!    `quarantined` are states no live driver rests in — something moved the row
//!    there and then stopped. Resume unconditionally.
//! 2. **The owner is gone.** The task run reached a terminal status (or its row
//!    is gone entirely), or — for a row that is actually holding lifted CPU —
//!    the exact Pod UID is confirmed absent from a fresh apiserver read.
//!
//! Both facts are established without asking the worker anything. The task-run
//! status is written by the coordinator's own Job/session reconciliation, and
//! Pod absence is read from the apiserver. Neither can be withheld by a process
//! that has already died.
//!
//! # Two ledgers
//!
//! `BuildLeaseService::release` writes `build_leases`. The resize lifecycle
//! lives in `build_pod_permits`. This module is the only thing that can ever
//! release the first for a worker that never sent `release_lease`, and it does
//! so **only** after the drop settled on a fact the cluster reported: 250m read
//! back from `status.initContainerStatuses`, or the exact Pod UID confirmed
//! absent. [`ResizeDropOutcome::releases_lease`] is that gate, and it is the
//! same gate the `release_lease` handler applies — not a second copy of the
//! rule.
//!
//! # No forked machinery
//!
//! The retry schedule, the 30-second confirmation budget, the fresh-GET fences
//! and the UID-fenced deletion all live in [`crate::task_run_resize_drop`], and
//! this module reaches them through
//! [`TaskRunResizeDropBridge::resize_drop_mechanism`] — the same constructor
//! the `release_lease` handler uses. Changing a backoff constant there changes
//! this module's observed timing, which is what keeps the two from drifting.
//!
//! # The gate is on the ACTION
//!
//! [`spawn`] is called unconditionally from `AppState::become_leader` and the
//! mode is re-read on **every tick**. Gating the wiring instead — returning
//! early before the loop exists — is how this repository has repeatedly shipped
//! a subsystem that was merged, green, and permanently unreachable. A disarmed
//! reconciler here is a loop that ticks and declines to act, and arming it is a
//! configuration change rather than a restart.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use djinn_coordinator::build_lease::BuildLeaseService;
use djinn_db::{
    BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository, BuildPodPermitRepository,
    BuildPodPermitRow, BuildPodPermitState, Database, ReleaseBuildPodPermitResult,
    TaskRunRepository, TransitionBuildPodResizeLifecycleResult,
};
use djinn_supervisor::services::{
    LeaseFencingToken, LeaseIdentity, LeaseReleaseRequest, LeaseResult, TaskInvocationLeaseIdentity,
};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::server::AppState;
use crate::task_run_resize_drop::{
    ResizeDropCause, ResizeDropOutcome, ResizeDropRequest, TaskRunResizeDropBridge,
};

/// How often the sweep runs when no override is configured.
///
/// A worker that dies at minute 40 of a live server produces a stranded row
/// that no startup scan will ever see, so the cadence — not the startup pass —
/// is what makes this reconciler self-healing. Thirty seconds is short relative
/// to the CPU an unreleased lift withholds and long relative to one durable
/// SELECT over a table with a handful of nonterminal rows.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How long one row's resumption may occupy a pass before it is left for the
/// next tick.
///
/// This bounds the PASS, never the drop's own safety properties: a resumption
/// that runs out of budget leaves the durable row exactly where it was, holding
/// the lease, and the next tick picks it up again. The quarantine absence loop
/// is deliberately unbounded inside `task_run_resize_drop`, which is precisely
/// why the reconciler must impose a bound out here instead — otherwise one
/// unreachable Pod would starve every other stranded row in the sweep.
pub const DEFAULT_ROW_BUDGET: Duration = Duration::from_secs(45);

/// Environment variable naming the reconciler's per-tick mode.
pub const MODE_ENV: &str = "DJINN_RESIZE_RECONCILE";

/// Environment variable overriding [`DEFAULT_SWEEP_INTERVAL`], in seconds.
pub const INTERVAL_ENV: &str = "DJINN_RESIZE_RECONCILE_INTERVAL_SECONDS";

// ── The per-tick gate ──────────────────────────────────────────────────────

/// What the reconciler is permitted to do on one tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResizeReconcileMode {
    /// Scan nothing and act on nothing. The loop still ticks.
    Off,
    /// Scan and classify, and log what would be resumed, but issue no PATCH,
    /// no DELETE and no lease release.
    Observe,
    /// Scan, classify and resume.
    #[default]
    Enforce,
}

impl ResizeReconcileMode {
    /// Parse the configured spelling. Anything unrecognised is
    /// [`Self::Enforce`], because the failure this module exists to prevent is
    /// a stranded Pod holding CPU nobody is accounting for, and a typo in a
    /// deployment value must not silently buy that.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "disabled" => Self::Off,
            "observe" | "dry_run" | "dry-run" => Self::Observe,
            _ => Self::Enforce,
        }
    }

    /// The stable spelling for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }

    /// Whether this mode may issue a PATCH, a DELETE or a lease release.
    #[must_use]
    pub const fn acts(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// The mode, re-read once per tick.
///
/// Behind a trait for exactly one reason: the property acceptance criterion 2
/// names is that the loop *asks again on every tick*, and a test that could only
/// observe a value captured at spawn time cannot tell a per-tick gate from a
/// wiring gate. A fake that flips between ticks is what makes the difference
/// observable without mutating a process-global environment variable that
/// sibling tests share.
pub trait ResizeReconcileGate: Send + Sync {
    /// The mode this tick runs under.
    fn mode(&self) -> ResizeReconcileMode;
}

/// The production gate: reads [`MODE_ENV`] on every call.
#[derive(Debug, Default)]
pub struct EnvResizeReconcileGate;

impl ResizeReconcileGate for EnvResizeReconcileGate {
    fn mode(&self) -> ResizeReconcileMode {
        std::env::var(MODE_ENV).map_or(ResizeReconcileMode::default(), |raw| {
            ResizeReconcileMode::parse(&raw)
        })
    }
}

/// The configured sweep cadence.
#[must_use]
pub fn sweep_interval_from_env() -> Duration {
    std::env::var(INTERVAL_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_SWEEP_INTERVAL, Duration::from_secs)
}

// ── What one pass did ──────────────────────────────────────────────────────

/// Why a scanned row was left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The task run is still live and the Pod is still present, so a live
    /// driver owns this row.
    OwnerLive,
    /// The permit carries no resize identity, so there is no container limit to
    /// return.
    NotResizeGoverned,
    /// Another driver moved the row out from under this pass's claim.
    ClaimLost,
    /// The durable read for this row failed; the next tick retries.
    Unreadable,
}

/// Why a row is — or is not — the reconciler's responsibility.
///
/// A bool would have been enough to decide whether to act. It is not enough to
/// decide what to do AFTERWARDS: a permit whose task run has ended is dead
/// weight that nothing else will ever retire, while one whose run is still live
/// belongs to the dispatch path. Collapsing the two into "stranded" is how a
/// reconciler ends up either leaking permit rows forever or retiring one out
/// from under a live build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strandedness {
    /// A live driver owns this row.
    Live,
    /// The task run reached a terminal status, or its row is gone.
    OwnerGone,
    /// The exact Pod UID is confirmed absent.
    PodGone,
    /// The row is parked in a state no live driver rests in.
    DropOwed,
}

impl Strandedness {
    const fn acts(self) -> bool {
        !matches!(self, Self::Live)
    }
}

/// The observable result of one sweep.
///
/// Every field is a count of something that was *done*, never of something that
/// was *considered*: a pass that scanned ten rows and resumed none reports
/// `resumed = 0`, which is what lets a test tell an idle loop from a working one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResizeReconcilePass {
    /// The mode this pass ran under.
    pub mode: Option<ResizeReconcileMode>,
    /// Rows `list_nonterminal_resize` returned.
    pub scanned: usize,
    /// Rows this pass drove a drop or quarantine for.
    pub resumed: usize,
    /// Rows an [`ResizeReconcileMode::Observe`] pass would have resumed.
    pub would_resume: usize,
    /// Rows left alone, with the reason.
    pub skipped: Vec<(String, SkipReason)>,
    /// Rows whose drop settled on a fact that permits releasing the durable
    /// `build_leases` row, and for which that release was issued.
    pub leases_released: usize,
    /// Rows whose `build_pod_permits` permit this pass retired, because nothing
    /// else ever will.
    pub permits_retired: usize,
    /// Rows whose resumption did not settle inside [`DEFAULT_ROW_BUDGET`].
    pub unsettled: usize,
    /// `true` when the durable scan itself failed.
    pub scan_failed: bool,
}

// ── The reconciler ─────────────────────────────────────────────────────────

/// The externally owned resumption of `build_pod_permits`' resize lifecycle.
pub struct TaskRunResizeReconciler {
    permits: Arc<BuildPodPermitRepository>,
    task_runs: TaskRunRepository,
    lease_rows: BuildLeaseRepository,
    leases: Option<Arc<BuildLeaseService>>,
    drop_bridge: Arc<TaskRunResizeDropBridge>,
    gate: Arc<dyn ResizeReconcileGate>,
    row_budget: Duration,
    /// Task runs a pass in this process is currently driving.
    ///
    /// Two invocations of one task run can be concurrent — the runner is
    /// `Arc`-shared behind `&self` with no lock — and two overlapping ticks
    /// would be a third driver. This is not a substitute for the durable CAS
    /// (which is what fences a driver in ANOTHER process); it is what stops this
    /// process from racing itself into two PATCHes for one drop.
    in_flight: Mutex<HashSet<String>>,
    passes: AtomicU64,
}

impl TaskRunResizeReconciler {
    /// Wire a reconciler to the durable relations and the fail-safe drop.
    #[must_use]
    pub fn new(
        db: Database,
        drop_bridge: Arc<TaskRunResizeDropBridge>,
        leases: Option<Arc<BuildLeaseService>>,
        gate: Arc<dyn ResizeReconcileGate>,
    ) -> Self {
        Self {
            permits: Arc::new(BuildPodPermitRepository::new(db.clone())),
            task_runs: TaskRunRepository::new(db.clone()),
            lease_rows: BuildLeaseRepository::new(db),
            leases,
            drop_bridge,
            gate,
            row_budget: DEFAULT_ROW_BUDGET,
            in_flight: Mutex::new(HashSet::new()),
            passes: AtomicU64::new(0),
        }
    }

    /// Override the per-row budget.
    #[must_use]
    pub const fn with_row_budget(mut self, budget: Duration) -> Self {
        self.row_budget = budget;
        self
    }

    /// How many passes this reconciler has run, whatever each one decided.
    ///
    /// A disarmed pass still increments it. That is the point: it is how a test
    /// distinguishes "the loop is running and declining to act" from "the loop
    /// was never spawned", which is the whole of acceptance criterion 2.
    #[must_use]
    pub fn passes(&self) -> u64 {
        self.passes.load(Ordering::SeqCst)
    }

    /// One sweep over every nonterminal resize row.
    pub async fn run_pass(&self) -> ResizeReconcilePass {
        self.passes.fetch_add(1, Ordering::SeqCst);
        let mode = self.gate.mode();
        let mut pass = ResizeReconcilePass {
            mode: Some(mode),
            ..ResizeReconcilePass::default()
        };
        if mode == ResizeReconcileMode::Off {
            return pass;
        }

        // THE DURABLE READ. Not a cache, not an in-process registry of live
        // invocations: the rows Postgres holds right now. A restart destroys
        // every other source of this information and leaves this one intact.
        let rows = match self.permits.list_nonterminal_resize().await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(
                    %error,
                    "task_run_resize_reconcile: the nonterminal resize scan failed; \
                     stranded rows keep their lease until the next tick"
                );
                pass.scan_failed = true;
                return pass;
            }
        };
        pass.scanned = rows.len();

        for row in rows {
            let task_run_id = row.task_run_id.clone();
            if row.resize_identity.is_none() {
                pass.skipped
                    .push((task_run_id, SkipReason::NotResizeGoverned));
                continue;
            }
            let strandedness = match self.strandedness(&row).await {
                Ok(strandedness) if strandedness.acts() => strandedness,
                Ok(_) => {
                    pass.skipped.push((task_run_id, SkipReason::OwnerLive));
                    continue;
                }
                Err(()) => {
                    pass.skipped.push((task_run_id, SkipReason::Unreadable));
                    continue;
                }
            };
            if !mode.acts() {
                pass.would_resume += 1;
                info!(
                    task_run_id = %task_run_id,
                    state = ?row.state,
                    mode = mode.as_str(),
                    "task_run_resize_reconcile: would resume this stranded resize row"
                );
                continue;
            }
            let Some(_guard) = InFlightGuard::claim(&self.in_flight, &task_run_id) else {
                pass.skipped.push((task_run_id, SkipReason::ClaimLost));
                continue;
            };
            // THE CLAIM. A reconciler has no prior claim on a row that is
            // resting in a lift state: it must WIN the compare-and-swap into
            // `drop_required` before it touches the Pod. A driver that loses
            // returns here without issuing a single PATCH, which is what keeps
            // a concurrent worker and this reconciler from both actuating one
            // drop.
            if !self.claim(&row).await {
                pass.skipped.push((task_run_id, SkipReason::ClaimLost));
                continue;
            }
            pass.resumed += 1;
            let Some(outcome) = self.resume(&row).await else {
                pass.unsettled += 1;
                continue;
            };
            // THE PERMIT LEDGER MOVES FIRST. `build_pod_permits` is retired
            // before `build_leases` is released, which is the same ordering the
            // `release_lease` handler imposes: capacity may never be handed back
            // ahead of the lifecycle that proves the CPU came with it.
            if self.retire_permit(&row, &outcome, strandedness).await {
                pass.permits_retired += 1;
            }
            if outcome.releases_lease() && self.release_durable_lease(&row).await {
                pass.leases_released += 1;
            }
        }
        pass
    }

    /// Retire the `build_pod_permits` row when nothing else ever will.
    ///
    /// `acquire_build_pod_permit` in the dispatch seam records that "the resize
    /// lifecycle that reclaims a permit (lift, drop, quarantine, release)
    /// belongs to `0ppk-3`'s reconciler", and
    /// [`ResizeDropOutcome::PodUidAbsent`] says in as many words that it leaves
    /// the row nonterminal for this module. This is that.
    ///
    /// Retiring is also what makes the sweep TERMINATE. A row left at
    /// `birth_confirmed` with a dead owner is stranded again by the next tick's
    /// own predicate, so a reconciler that only ever dropped would re-drop the
    /// same dead task run every 30 seconds forever.
    async fn retire_permit(
        &self,
        row: &BuildPodPermitRow,
        outcome: &ResizeDropOutcome,
        strandedness: Strandedness,
    ) -> bool {
        let reason = match outcome {
            // The Pod is gone. `capture_resize_identity` is write-once and
            // `acquire` refuses to resurrect a released row, so this task run
            // can never capture a replacement — recovery is a NEW task run.
            ResizeDropOutcome::PodUidAbsent { .. } => "resize_pod_uid_absent",
            // The launcher is back at 250m, and the run that borrowed the CPU
            // has ended. Nothing will come back for this permit.
            ResizeDropOutcome::Confirmed { .. } | ResizeDropOutcome::NotResizeGoverned
                if strandedness != Strandedness::Live =>
            {
                if !matches!(strandedness, Strandedness::OwnerGone)
                    && !self.owner_is_gone(&row.task_run_id).await.unwrap_or(false)
                {
                    return false;
                }
                "resize_owner_gone"
            }
            // `QuarantinedPodDeleted` already released the permit inside the
            // drop; every other outcome is unsettled and must keep its row.
            _ => return false,
        };
        match self
            .permits
            .release(&row.task_run_id, &row.permit_id, row.fencing_token, reason)
            .await
        {
            Ok(ReleaseBuildPodPermitResult::Released(_)) => {
                info!(
                    task_run_id = %row.task_run_id,
                    reason,
                    "task_run_resize_reconcile: permit retired; this task run can never \
                     capture a replacement Pod — recovery is a NEW task run"
                );
                true
            }
            Ok(ReleaseBuildPodPermitResult::AlreadyReleased(_)) => false,
            Ok(ReleaseBuildPodPermitResult::Rejected) => {
                warn!(
                    task_run_id = %row.task_run_id,
                    "task_run_resize_reconcile: the permit refused retirement"
                );
                false
            }
            Err(error) => {
                warn!(
                    task_run_id = %row.task_run_id,
                    %error,
                    "task_run_resize_reconcile: the permit could not be retired"
                );
                false
            }
        }
    }

    /// Whether this row is the reconciler's responsibility.
    ///
    /// `Err(())` means the evidence could not be read at all, which is never
    /// grounds to act: an unanswered database is not proof that a worker died.
    async fn strandedness(&self, row: &BuildPodPermitRow) -> Result<Strandedness, ()> {
        match row.state {
            // A drop is already owed. No live driver rests in these states —
            // something moved the row here and then stopped, which is exactly
            // the shape a dead worker or a restarted server leaves behind.
            BuildPodPermitState::DropRequired
            | BuildPodPermitState::DropApplying
            | BuildPodPermitState::Quarantined => Ok(Strandedness::DropOwed),
            BuildPodPermitState::BirthConfirmed
            | BuildPodPermitState::LiftApplying
            | BuildPodPermitState::Lifted => {
                if self.owner_is_gone(&row.task_run_id).await? {
                    return Ok(Strandedness::OwnerGone);
                }
                // A row that is HOLDING lifted CPU earns one fresh apiserver
                // read even while its task run still reads live: a Pod whose
                // node vanished leaves the run's status behind for as long as
                // the coordinator takes to notice, and the ceiling is withheld
                // for the whole of that window.
                if matches!(
                    row.state,
                    BuildPodPermitState::LiftApplying | BuildPodPermitState::Lifted
                ) && self.pod_uid_absent(row).await
                {
                    return Ok(Strandedness::PodGone);
                }
                Ok(Strandedness::Live)
            }
            // Not in the nonterminal resize set at all; nothing to resume.
            BuildPodPermitState::Acquired
            | BuildPodPermitState::JobCreated
            | BuildPodPermitState::Released => Ok(Strandedness::Live),
        }
    }

    /// Whether the task run that owns this permit has ended.
    ///
    /// Written by the coordinator's own Job and session reconciliation, so it
    /// is available even when every line of code in the worker has stopped
    /// running.
    async fn owner_is_gone(&self, task_run_id: &str) -> Result<bool, ()> {
        match self.task_runs.get(task_run_id).await {
            // No task run row at all: nothing will ever drive this permit.
            Ok(None) => Ok(true),
            Ok(Some(run)) => Ok(matches!(
                run.status.as_str(),
                "completed" | "failed" | "interrupted"
            )),
            Err(error) => {
                warn!(
                    %task_run_id,
                    %error,
                    "task_run_resize_reconcile: task-run status unreadable; not acting"
                );
                Err(())
            }
        }
    }

    /// A fresh, UID-fenced read: is the exact Pod this lift was bound to gone?
    ///
    /// An apiserver that does not answer reports `false` — "not proven absent"
    /// — because an unreachable apiserver is not evidence that a worker died,
    /// and acting on it would drop a live build's ceiling.
    async fn pod_uid_absent(&self, row: &BuildPodPermitRow) -> bool {
        let Some(identity) = row.resize_identity.as_ref() else {
            return false;
        };
        let Ok(surface) = self.drop_bridge.resize_pod_surface().await else {
            return false;
        };
        match surface.observe_launcher(&row.task_run_id).await {
            Ok(None) => true,
            Ok(Some(observed)) => observed.pod_uid != identity.pod_uid,
            Err(_) => false,
        }
    }

    /// Move a resting row into `drop_required`, fenced on the full tuple.
    ///
    /// Rows already at `drop_required`, `drop_applying` or `quarantined` need no
    /// claim — the transition would be a self-transition and the drop resumes
    /// them idempotently.
    async fn claim(&self, row: &BuildPodPermitRow) -> bool {
        let from = match row.state {
            state @ (BuildPodPermitState::BirthConfirmed
            | BuildPodPermitState::LiftApplying
            | BuildPodPermitState::Lifted) => state,
            _ => return true,
        };
        let Some(identity) = row.resize_identity.as_ref() else {
            return false;
        };
        match self
            .permits
            .transition_resize_lifecycle(
                &row.task_run_id,
                &row.permit_id,
                row.fencing_token,
                &identity.pod_uid,
                row.resize_invocation_id.as_deref(),
                from,
                BuildPodPermitState::DropRequired,
            )
            .await
        {
            Ok(TransitionBuildPodResizeLifecycleResult::Transitioned(_)) => true,
            Ok(TransitionBuildPodResizeLifecycleResult::Rejected) => {
                info!(
                    task_run_id = %row.task_run_id,
                    from = ?from,
                    "task_run_resize_reconcile: another driver holds this row; not patching"
                );
                false
            }
            Err(error) => {
                warn!(
                    task_run_id = %row.task_run_id,
                    %error,
                    "task_run_resize_reconcile: the claim could not be written"
                );
                false
            }
        }
    }

    /// Drive the fail-safe drop for one stranded row.
    ///
    /// `invocation_id: None` is deliberate and is the infrastructure-observer
    /// shape `task_run_resize_drop` documents: the reconciler did not perform
    /// the lift and has no invocation of its own to name, so it drives whichever
    /// one the durable row carries — and every durable write below it is still
    /// fenced on that invocation, so a driver holding a different one is
    /// rejected without a PATCH.
    async fn resume(&self, row: &BuildPodPermitRow) -> Option<ResizeDropOutcome> {
        let mechanism = match self.drop_bridge.resize_drop_mechanism().await {
            Ok(mechanism) => mechanism,
            Err(error) => {
                warn!(
                    task_run_id = %row.task_run_id,
                    %error,
                    "task_run_resize_reconcile: no apiserver surface; the lease stays held"
                );
                return Some(ResizeDropOutcome::Unavailable(error));
            }
        };
        let request = ResizeDropRequest {
            task_run_id: row.task_run_id.clone(),
            invocation_id: None,
            cause: ResizeDropCause::ServerRestart,
        };
        info!(
            task_run_id = %row.task_run_id,
            state = ?row.state,
            invocation_id = ?row.resize_invocation_id,
            "task_run_resize_reconcile: resuming a stranded resize row from persisted state"
        );
        match tokio::time::timeout(self.row_budget, mechanism.drop_to_birth(&request)).await {
            Ok(outcome) => Some(outcome),
            Err(_) => {
                warn!(
                    task_run_id = %row.task_run_id,
                    budget = ?self.row_budget,
                    "task_run_resize_reconcile: resumption did not settle in this pass; the \
                     durable row keeps its state and the lease stays held"
                );
                None
            }
        }
    }

    /// THE OTHER LEDGER. Release the durable `build_leases` row this permit's
    /// invocation is holding.
    ///
    /// Only ever reached from an outcome whose `releases_lease()` is true, which
    /// means the cluster reported either 250m in `status.initContainerStatuses`
    /// or the exact Pod UID absent. Writing `build_pod_permits` does not release
    /// capacity, and saying "the lease was released" without naming the store is
    /// how a success log comes from the wrong ledger.
    async fn release_durable_lease(&self, row: &BuildPodPermitRow) -> bool {
        let Some(leases) = self.leases.as_ref() else {
            return false;
        };
        let Some(invocation_id) = row.resize_invocation_id.clone() else {
            // No invocation ever claimed this row, so no per-invocation lease
            // was ever taken against it.
            return false;
        };
        let task_id = match self.task_runs.get(&row.task_run_id).await {
            Ok(Some(run)) => run.task_id,
            Ok(None) | Err(_) => return false,
        };
        let key = BuildLeaseKey {
            consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
            consumer_id: invocation_id.clone(),
        };
        let Ok(Some(lease)) = self.lease_rows.get(&key).await else {
            return false;
        };
        let Some(token) = lease.fencing_token else {
            return false;
        };
        let result = leases
            .release(LeaseReleaseRequest {
                identity: LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
                    task_id,
                    task_run_id: row.task_run_id.clone(),
                    invocation_id: invocation_id.clone(),
                }),
                fencing_token: LeaseFencingToken(token.unsigned_abs()),
                candidate_cleanup: false,
            })
            .await;
        let released = matches!(result, LeaseResult::Released { .. });
        if released {
            info!(
                task_run_id = %row.task_run_id,
                %invocation_id,
                "task_run_resize_reconcile: the launcher is back at its birth limit or gone; \
                 the durable build_leases row is released"
            );
        } else {
            warn!(
                task_run_id = %row.task_run_id,
                %invocation_id,
                ?result,
                "task_run_resize_reconcile: build_leases refused the release; retrying next tick"
            );
        }
        released
    }
}

/// A process-local claim on one task run's resumption, released on drop.
struct InFlightGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    task_run_id: String,
}

impl<'a> InFlightGuard<'a> {
    fn claim(set: &'a Mutex<HashSet<String>>, task_run_id: &str) -> Option<Self> {
        let mut guard = set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.insert(task_run_id.to_owned()) {
            return None;
        }
        Some(Self {
            set,
            task_run_id: task_run_id.to_owned(),
        })
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .set
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&self.task_run_id);
    }
}

// ── The production composition ─────────────────────────────────────────────

/// Start the resize reconciler. Leader-only: composed exclusively from
/// `AppState::become_leader`.
///
/// Leader-only because it WRITES — it moves `build_pod_permits` lifecycle rows,
/// PATCHes and DELETEs Pods, and releases `build_leases` rows. A standby pod
/// running this concurrently would give one stranded Pod two drivers, which is
/// the single-active-writer invariant the coordinator advisory lock exists to
/// hold.
///
/// **The spawn is unconditional.** Whether a tick acts is decided per tick by
/// [`EnvResizeReconcileGate`]. Moving that decision out here — returning before
/// the loop exists — would leave a module nothing calls and a green test suite,
/// which is the exact failure the comment above `scip_index_watcher::spawn` in
/// `AppState::become_leader` records.
pub fn spawn(state: AppState) {
    // Everything AppState-derived is extracted here and the handle released
    // immediately: what survives into the background is a pool handle, two
    // Arcs and a cancellation token, never the coordinator or the git actors.
    let db = state.db().clone();
    let cancel = state.cancel().clone();
    let drop_bridge = state.resize_drop();
    let leases = state.build_lease();
    drop(state);

    let reconciler = Arc::new(TaskRunResizeReconciler::new(
        db,
        drop_bridge,
        Some(leases),
        Arc::new(EnvResizeReconcileGate),
    ));
    let interval = sweep_interval_from_env();
    tokio::spawn(async move {
        info!(
            ?interval,
            "task_run_resize_reconcile: started (leader-only); the mode is re-read every tick"
        );
        run_loop(interval, cancel, move || {
            let reconciler = Arc::clone(&reconciler);
            async move {
                let pass = reconciler.run_pass().await;
                if pass.resumed > 0 || pass.would_resume > 0 || pass.scan_failed {
                    info!(
                        mode = pass.mode.map(ResizeReconcileMode::as_str),
                        scanned = pass.scanned,
                        resumed = pass.resumed,
                        would_resume = pass.would_resume,
                        leases_released = pass.leases_released,
                        unsettled = pass.unsettled,
                        scan_failed = pass.scan_failed,
                        "task_run_resize_reconcile: sweep"
                    );
                }
            }
        })
        .await;
        warn!(
            "task_run_resize_reconcile: the sweep loop stopped; stranded resize rows will not \
             be resumed by this process"
        );
    });
}

/// The ticker seam production and the cancellation tests share.
///
/// The FIRST tick fires immediately, before any interval has elapsed: that is
/// the startup scan, and it is the same code path as every later sweep rather
/// than a separate one-shot. A startup-only reaper paired with a read-only
/// doctor is how this repository produced a phantom-active state that never
/// self-healed; there is deliberately only one scan implementation here, and it
/// is periodic.
pub async fn run_loop<F, Fut>(interval: Duration, cancel: CancellationToken, mut tick: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticker.tick() => tick().await,
        }
    }
}

#[cfg(test)]
#[path = "task_run_resize_reconcile_tests.rs"]
mod tests;
