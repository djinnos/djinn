//! Kueue Workload reconciliation — the leader-only reflector that maps Kueue's
//! admission decisions onto task-run state.
//!
//! This occupies the seam vacated by `build_admission_reconcile.rs`, and the
//! swap is not like-for-like. That module POLLED a Postgres ledger djinn wrote
//! itself, on a 120s timer, to retire rows whose Kubernetes objects had
//! vanished. Kueue's ClusterQueue is the capacity authority now, so there is no
//! ledger to retire — there is an external decision-maker whose decisions this
//! process needs to hear about.
//!
//! # Why a watch and not a poll
//!
//! Admission is edge-triggered and REVERSIBLE. Kueue admits a Workload, preempts
//! it for quota, and admits it again; each of those edges changes what the run
//! behind it is doing. A poll on any cadence collapses an admit/evict/admit
//! sequence into whichever sample it happened to take, and the edge it drops —
//! the eviction — is the one that leaves a task-run live in the database with no
//! Pod behind it.
//!
//! Modelled on `djinn-image-controller/src/watcher.rs`, the repository's only
//! real `kube::runtime::watcher` loop. Deliberately NOT modelled on
//! `djinn-k8s/src/runtime.rs`'s `watch_infra_death`, which despite the name is a
//! GET loop on a timer.
//!
//! # What it does on a cluster with no Kueue installed
//!
//! Nothing, and it says so once.
//!
//! [`spawn`] refuses to open a watch unless three things hold: the runtime is
//! Kubernetes, `KubernetesConfig::kueue_armed` is true, and a client can be
//! built. `kueue.armed` defaults false, no namespace carries
//! `djinn.io/kueue-managed`, and therefore production today has no Kueue CRDs and
//! no Workload objects at all. On that cluster this module logs one line at
//! startup and spawns no watch, no timer and no task.
//!
//! That is a deliberate gate on the WIRING rather than on the ACTION, with one
//! specific justification: an armed watch against a cluster whose `Workload`
//! resource is unregistered gets `404 NotFound` from the API server on every
//! reconnect, forever. Backing that off is correct but it is still a permanent
//! error loop over a resource that will never appear, and a permanent error loop
//! is how a real signal gets filtered out. If Kueue IS armed and the CRDs are
//! missing, the loop runs, backs off, and complains — because then the CRDs
//! being missing is a genuine outage.
//!
//! # Lifecycle: the reflector holds a `Weak`, never an `Arc`
//!
//! A detached loop that holds a strong `Arc` to shared state keeps that state
//! alive past the shutdown that was supposed to drop it. This repository has
//! already paid for that once: `RpcServices` owns the worker's single outbound
//! frame `Sender`, orderly shutdown blocks until the last clone is gone, and one
//! strong clone parked in a background sweep wedged the worker's exit for 30
//! seconds (see `djinn-agent/src/context.rs:152`).
//!
//! So the split here is:
//!
//! * the SUPERVISOR task owns the one strong [`Arc<Reconciler>`]. It does
//!   nothing but wait for the leader's cancellation token and then drop it. That
//!   drop is the leadership lease being released.
//! * the REFLECTOR task owns only a [`Weak<Reconciler>`]. It upgrades per
//!   observation and releases the strong handle before it goes back to waiting,
//!   so it never holds one across an await. When the supervisor's drop makes the
//!   upgrade fail, the reflector returns [`ReflectorExit::LeadershipReleased`]
//!   and its task is dropped.
//!
//! Two independent stop signals — the token and the dead `Weak` — is deliberate.
//! A cancellation that is somehow never observed still stops the reflector,
//! because the state it needs is gone.

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use djinn_agent::runtime_bridge::{RuntimeKind, runtime_kind};
use djinn_core::models::TaskRunStatus;
use djinn_db::repositories::task_run::{TaskRunRepository, TerminalStatusAcceptance};
use djinn_db::{AdmissionApplied, Database, KueueWorkloadAdmissionRepository};
use djinn_k8s::workload_inventory::{
    KubeWorkloadWatch, WorkloadAdmission, WorkloadObservation, WorkloadWatch,
    classify_workload_admission, workload_task_run_id,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::server::AppState;

/// How long to wait before re-opening a watch session that ended or broke.
///
/// Matches `djinn-image-controller`'s `POST_ERROR_SLEEP`. Short enough that a
/// routine API-server rollout costs one reconnect, long enough that an
/// unregistered CRD does not become a hot loop.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Depth of the channel between one watch session and the reconciler.
///
/// Bounded on purpose: a slow sink must apply backpressure to the watch rather
/// than buffering an unbounded replay in memory. `kube`'s watcher handles being
/// polled slowly; it cannot handle this process running out of memory.
const OBSERVATION_BUFFER: usize = 64;

// =============================================================================
// The sink
// =============================================================================

/// What one applied observation did to durable state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkOutcome {
    /// The projection moved. This is the only outcome that counts as a
    /// transition.
    Transitioned {
        previous: Option<String>,
        interrupted_run: bool,
    },
    /// The projection already held this state — a watch replay, or a Workload
    /// update that touched something this process does not read.
    Unchanged,
}

/// Where Workload admission state lands.
///
/// A trait so the reflector's lifecycle can be exercised without a database.
/// It is NOT a seam for the state tests: acceptance of "admitted drives the run
/// out of pending" is asserted against [`DurableAdmissionSink`] and a real
/// database, because a fake sink can only prove the reflector called something.
#[async_trait::async_trait]
pub trait TaskRunAdmissionSink: Send + Sync + 'static {
    async fn apply(
        &self,
        task_run_id: &str,
        admission: &WorkloadAdmission,
        workload_name: Option<&str>,
    ) -> Result<SinkOutcome, String>;

    /// The Workload is gone. Kueue garbage-collects it with its owning Job, so
    /// this is a lifecycle end and the projection row must go with it.
    async fn forget(&self, task_run_id: &str) -> Result<(), String>;
}

/// The production sink: the durable projection, plus the one action a Kueue
/// decision demands of djinn.
pub struct DurableAdmissionSink {
    admissions: KueueWorkloadAdmissionRepository,
    task_runs: TaskRunRepository,
}

impl DurableAdmissionSink {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self {
            admissions: KueueWorkloadAdmissionRepository::new(db.clone()),
            task_runs: TaskRunRepository::new(db),
        }
    }
}

#[async_trait::async_trait]
impl TaskRunAdmissionSink for DurableAdmissionSink {
    async fn apply(
        &self,
        task_run_id: &str,
        admission: &WorkloadAdmission,
        workload_name: Option<&str>,
    ) -> Result<SinkOutcome, String> {
        let applied = self
            .admissions
            .apply(
                task_run_id,
                admission.as_str(),
                admission.reason(),
                workload_name,
            )
            .await
            .map_err(|e| e.to_string())?;

        let previous = match applied {
            AdmissionApplied::Unchanged => return Ok(SinkOutcome::Unchanged),
            AdmissionApplied::Recorded => None,
            AdmissionApplied::Transitioned { previous } => Some(previous),
        };

        // A quota eviction of an ADMITTED Workload is the one edge that has to
        // reach beyond the projection. Kueue re-suspends the Job and deletes its
        // Pod; the `task_runs` row the in-pod supervisor created stays `running`
        // with nothing behind it, and nothing else in the process will notice
        // for the length of the generic stall reaper's window. Terminalising it
        // as `Interrupted` is what returns the task to the dispatchable
        // population — and `Interrupted` specifically, because this is
        // infrastructure taking the pod away, not the attempt failing.
        //
        // Gated on the DIRECTION, not on the destination. A Workload that has
        // only ever been queued has no run to interrupt, and interrupting on
        // every `pending` observation would kill runs that were never admitted.
        let eviction = admission.is_pending() && previous.as_deref() == Some("admitted");
        let mut interrupted_run = false;
        if eviction {
            match self
                .task_runs
                .accept_terminal_status(task_run_id, TaskRunStatus::Interrupted)
                .await
            {
                Ok(TerminalStatusAcceptance::Accepted) => {
                    interrupted_run = true;
                    tracing::warn!(
                        task_run_id,
                        reason = admission.reason().unwrap_or("unspecified"),
                        "kueue_workload_reconcile: admitted Workload evicted; the live task-run \
                         was interrupted so its task returns to the dispatchable pool"
                    );
                }
                // No live row: the Workload was evicted before the pod ever
                // created one, or the run already finished. Both are normal and
                // neither is an error.
                Ok(other) => {
                    tracing::info!(
                        task_run_id,
                        acceptance = ?other,
                        "kueue_workload_reconcile: Workload evicted with no live task-run to \
                         interrupt"
                    );
                }
                Err(e) => {
                    return Err(format!("interrupting evicted task-run {task_run_id}: {e}"));
                }
            }
        }

        Ok(SinkOutcome::Transitioned {
            previous,
            interrupted_run,
        })
    }

    async fn forget(&self, task_run_id: &str) -> Result<(), String> {
        self.admissions
            .forget(task_run_id)
            .await
            .map_err(|e| e.to_string())
    }
}

// =============================================================================
// The reconciler
// =============================================================================

/// The leadership-scoped state the reflector operates on.
///
/// Held strongly by exactly one place — the supervisor task in [`spawn`] — so
/// that dropping it is an unambiguous "this process is no longer the leader".
pub struct Reconciler {
    sink: Arc<dyn TaskRunAdmissionSink>,
    /// Observed state CHANGES applied. Never advanced by a replay.
    transitions: AtomicU64,
    /// Observations handled, replays included.
    observations: AtomicU64,
    /// Workloads that resolved to no task-run and were left alone.
    ignored: AtomicU64,
    /// Watch sessions (re)established.
    sessions: AtomicU64,
}

impl Reconciler {
    #[must_use]
    pub fn new(sink: Arc<dyn TaskRunAdmissionSink>) -> Self {
        Self {
            sink,
            transitions: AtomicU64::new(0),
            observations: AtomicU64::new(0),
            ignored: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
        }
    }

    /// Durable state changes applied since this reconciler was created.
    #[must_use]
    pub fn transitions(&self) -> u64 {
        self.transitions.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn observations(&self) -> u64 {
        self.observations.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn ignored(&self) -> u64 {
        self.ignored.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn sessions(&self) -> u64 {
        self.sessions.load(Ordering::SeqCst)
    }

    /// Apply one observation.
    ///
    /// Never returns an error: a Workload this process cannot act on must not be
    /// able to end the watch. Failures are logged and the stream continues.
    pub async fn handle(&self, observation: WorkloadObservation) {
        self.observations.fetch_add(1, Ordering::SeqCst);
        match observation {
            WorkloadObservation::SessionRestarted => {
                self.sessions.fetch_add(1, Ordering::SeqCst);
                tracing::debug!(
                    "kueue_workload_reconcile: watch session established; replaying current state"
                );
            }
            WorkloadObservation::Applied(workload) => {
                // A Workload that is not ours resolves to nothing and MUST leave
                // every task-run untouched. Warm Jobs and SCIP Jobs carry the
                // same build-object label and reach this same stream.
                let Some(task_run_id) = workload_task_run_id(&workload) else {
                    self.ignored.fetch_add(1, Ordering::SeqCst);
                    tracing::debug!(
                        workload = workload.metadata.name.as_deref().unwrap_or("<unnamed>"),
                        "kueue_workload_reconcile: Workload resolves to no task-run; ignored"
                    );
                    return;
                };
                let admission = classify_workload_admission(&workload);
                let name = workload.metadata.name.clone();
                match self
                    .sink
                    .apply(&task_run_id, &admission, name.as_deref())
                    .await
                {
                    Ok(SinkOutcome::Transitioned {
                        previous,
                        interrupted_run,
                    }) => {
                        self.transitions.fetch_add(1, Ordering::SeqCst);
                        tracing::info!(
                            task_run_id,
                            previous = previous.as_deref().unwrap_or("<new>"),
                            admission = admission.as_str(),
                            reason = admission.reason().unwrap_or("unspecified"),
                            interrupted_run,
                            "kueue_workload_reconcile: task-run admission state moved"
                        );
                    }
                    Ok(SinkOutcome::Unchanged) => {}
                    Err(error) => {
                        tracing::warn!(
                            task_run_id,
                            %error,
                            "kueue_workload_reconcile: failed to apply Workload admission state; \
                             the next observation retries"
                        );
                    }
                }
            }
            WorkloadObservation::Deleted(workload) => {
                let Some(task_run_id) = workload_task_run_id(&workload) else {
                    self.ignored.fetch_add(1, Ordering::SeqCst);
                    return;
                };
                if let Err(error) = self.sink.forget(&task_run_id).await {
                    tracing::warn!(
                        task_run_id,
                        %error,
                        "kueue_workload_reconcile: failed to drop the projection for a deleted \
                         Workload"
                    );
                }
            }
        }
    }
}

// =============================================================================
// The reflector
// =============================================================================

/// Why the reflector stopped. Both variants are ordinary; neither is a fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflectorExit {
    /// The leader's cancellation token fired.
    Cancelled,
    /// Every strong handle to the [`Reconciler`] is gone — leadership released.
    LeadershipReleased,
}

/// Watch Kueue Workloads until leadership ends, re-establishing the session
/// whenever it drops.
///
/// `reconciler` is a [`Weak`] BY CONTRACT, not by convenience. See the module
/// docs: a strong handle parked here would keep leadership state alive across
/// shutdown, and a "did the loop stop?" assertion against a strong reference
/// passes whether or not that happened.
pub(crate) async fn run_reflector(
    reconciler: Weak<Reconciler>,
    watch: Arc<dyn WorkloadWatch>,
    cancel: CancellationToken,
    backoff: Duration,
) -> ReflectorExit {
    loop {
        if cancel.is_cancelled() {
            return ReflectorExit::Cancelled;
        }
        if reconciler.upgrade().is_none() {
            return ReflectorExit::LeadershipReleased;
        }

        let (tx, mut rx) = mpsc::channel(OBSERVATION_BUFFER);
        let session_watch = Arc::clone(&watch);
        let session = tokio::spawn(async move { session_watch.run_session(tx).await });

        let exit = loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    session.abort();
                    return ReflectorExit::Cancelled;
                }
                observation = rx.recv() => {
                    let Some(observation) = observation else { break None };
                    // Upgrade per observation and release before going back to
                    // waiting: a strong handle must never be held across the
                    // `recv()` await.
                    let Some(state) = reconciler.upgrade() else {
                        session.abort();
                        break Some(ReflectorExit::LeadershipReleased);
                    };
                    state.handle(observation).await;
                    drop(state);
                }
            }
        };
        if let Some(exit) = exit {
            return exit;
        }

        match session.await {
            Ok(Ok(())) => {
                tracing::debug!("kueue_workload_reconcile: watch session ended; re-establishing")
            }
            Ok(Err(error)) => tracing::warn!(
                %error,
                "kueue_workload_reconcile: watch session failed; re-establishing after backoff. \
                 A persistent 404 here means Kueue's Workload CRD is not registered in this \
                 cluster while djinn is armed for it."
            ),
            Err(join_error) if join_error.is_cancelled() => {}
            Err(join_error) => tracing::error!(
                error = %join_error,
                "kueue_workload_reconcile: watch session task died; re-establishing after backoff"
            ),
        }

        tokio::select! {
            () = cancel.cancelled() => return ReflectorExit::Cancelled,
            () = tokio::time::sleep(backoff) => {}
        }
    }
}

// =============================================================================
// Registration
// =============================================================================

/// Build the production watch, or explain why there is nothing to watch.
///
/// Returns `None` on every configuration where opening a watch would be wrong,
/// and says which one it was — a silent `None` here is indistinguishable from a
/// broken reconciler.
async fn production_watch() -> Option<Arc<dyn WorkloadWatch>> {
    if !matches!(runtime_kind(), RuntimeKind::Kubernetes) {
        tracing::debug!(
            "kueue_workload_reconcile: DJINN_RUNTIME is not kubernetes; no Workloads exist"
        );
        return None;
    }
    let config = djinn_k8s::KubernetesConfig::from_env();
    if !config.kueue_armed {
        tracing::info!(
            "kueue_workload_reconcile: Kueue is not armed (DJINN_KUEUE_ARMED unset/false), so no \
             Job is captured and no Workload exists. Not starting the Workload watch."
        );
        return None;
    }
    match djinn_k8s::try_default_client().await {
        Ok(client) => Some(Arc::new(KubeWorkloadWatch::new(client, config.namespace))),
        Err(error) => {
            tracing::error!(
                %error,
                "kueue_workload_reconcile: Kueue is ARMED but no Kubernetes client could be \
                 built. Admitted and evicted Workloads will not reach task-run state."
            );
            None
        }
    }
}

/// Start Workload reconciliation. Leader-only: composed exclusively from
/// `AppState::become_leader`, at the seam the deleted build-admission reconciler
/// used to occupy.
///
/// Leader-only because the sink WRITES — it moves the durable admission
/// projection and terminalises live task-runs. Standby HTTP-only pods running
/// this concurrently would give a preempted run two writers, which is exactly
/// the single-active-writer invariant the coordinator advisory lock exists to
/// hold.
pub fn spawn(state: AppState) {
    // Everything AppState-derived is extracted here and the handle released
    // immediately: what survives into the background is a `Database` (a pool
    // handle) and a `CancellationToken`, never the coordinator, the RPC
    // services or the git actors.
    let db = state.db().clone();
    let cancel = state.cancel().clone();
    drop(state);

    tokio::spawn(async move {
        let Some(watch) = production_watch().await else {
            return;
        };

        let reconciler = Arc::new(Reconciler::new(Arc::new(DurableAdmissionSink::new(db))));
        let reflector = tokio::spawn(run_reflector(
            Arc::downgrade(&reconciler),
            watch,
            cancel.clone(),
            RECONNECT_BACKOFF,
        ));
        tracing::info!("kueue_workload_reconcile: Workload reflector started (leader-only)");

        // This task holds the ONE strong handle. Waiting here and then dropping
        // it is the leadership lease; the reflector holds only a `Weak`.
        cancel.cancelled().await;
        drop(reconciler);
        match reflector.await {
            Ok(exit) => tracing::warn!(
                ?exit,
                "kueue_workload_reconcile: Workload reflector stopped; Kueue admission state no \
                 longer reaches task-run state in this process"
            ),
            Err(error) => tracing::error!(
                %error,
                "kueue_workload_reconcile: Workload reflector task died"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_k8s::workload_inventory::{
        CONDITION_ADMITTED, CONDITION_EVICTED, KueueWorkload, KueueWorkloadCondition,
        KueueWorkloadStatus, OwnerReference,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    // ── fixtures ──────────────────────────────────────────────────────────

    fn condition(kind: &str, status: &str, reason: Option<&str>) -> KueueWorkloadCondition {
        KueueWorkloadCondition {
            condition_type: kind.to_owned(),
            status: status.to_owned(),
            reason: reason.map(ToOwned::to_owned),
            message: None,
        }
    }

    /// A Workload Kueue derived from a task-run Job, in the given state.
    fn task_run_workload(
        task_run_id: &str,
        conditions: Vec<KueueWorkloadCondition>,
    ) -> KueueWorkload {
        let mut workload = KueueWorkload {
            status: KueueWorkloadStatus { conditions },
            ..Default::default()
        };
        workload.metadata.name = Some(format!("job-djinn-taskrun-{task_run_id}-abcde"));
        workload.metadata.labels = Some(BTreeMap::from([
            (
                djinn_k8s::job::LABEL_TASK_RUN_ID.to_owned(),
                task_run_id.to_owned(),
            ),
            (
                djinn_k8s::config::LABEL_KUEUE_BUILD_OBJECT.to_owned(),
                "true".to_owned(),
            ),
        ]));
        workload
    }

    fn admitted(task_run_id: &str) -> WorkloadObservation {
        WorkloadObservation::Applied(task_run_workload(
            task_run_id,
            vec![condition(CONDITION_ADMITTED, "True", Some("Admitted"))],
        ))
    }

    fn queued(task_run_id: &str) -> WorkloadObservation {
        WorkloadObservation::Applied(task_run_workload(
            task_run_id,
            vec![condition(
                CONDITION_ADMITTED,
                "False",
                Some("NoReservation"),
            )],
        ))
    }

    /// Kueue's shape for a quota preemption: `Evicted=True` layered on top of
    /// the `Admitted=True` it has not cleared yet.
    fn evicted_for_quota(task_run_id: &str) -> WorkloadObservation {
        WorkloadObservation::Applied(task_run_workload(
            task_run_id,
            vec![
                condition(CONDITION_ADMITTED, "True", Some("Admitted")),
                condition(CONDITION_EVICTED, "True", Some("Preempted")),
            ],
        ))
    }

    /// A warm Job's Workload: same build-object label, same watch, no task-run.
    fn warm_workload_evicted() -> WorkloadObservation {
        let mut workload = KueueWorkload {
            status: KueueWorkloadStatus {
                conditions: vec![condition(CONDITION_EVICTED, "True", Some("Preempted"))],
            },
            ..Default::default()
        };
        workload.metadata.name = Some("job-djinn-warm-proj-7-zzzzz".to_owned());
        workload.metadata.labels = Some(BTreeMap::from([(
            djinn_k8s::config::LABEL_KUEUE_BUILD_OBJECT.to_owned(),
            "true".to_owned(),
        )]));
        workload.metadata.owner_references = Some(vec![OwnerReference {
            kind: "Job".to_owned(),
            name: "djinn-warm-proj-7".to_owned(),
            ..Default::default()
        }]);
        WorkloadObservation::Applied(workload)
    }

    /// One scripted watch session: what it delivers, then how it ends.
    type ScriptedSession = (Vec<WorkloadObservation>, Result<(), String>);

    /// A scripted watch. Each element is one session: the observations it
    /// delivers, then how that session ends.
    struct ScriptedWatch {
        sessions: Mutex<std::collections::VecDeque<ScriptedSession>>,
        /// Repeated forever once the script is exhausted, so a lifecycle test
        /// can keep the loop turning without an ever-growing script.
        tail: Option<ScriptedSession>,
    }

    impl ScriptedWatch {
        fn new(sessions: Vec<ScriptedSession>, tail: Option<ScriptedSession>) -> Arc<Self> {
            Arc::new(Self {
                sessions: Mutex::new(sessions.into()),
                tail,
            })
        }
    }

    #[async_trait::async_trait]
    impl WorkloadWatch for ScriptedWatch {
        async fn run_session(&self, tx: mpsc::Sender<WorkloadObservation>) -> Result<(), String> {
            let session = {
                let mut sessions = self.sessions.lock().expect("scripted watch lock");
                sessions.pop_front().or_else(|| self.tail.clone())
            };
            let Some((observations, outcome)) = session else {
                // Script exhausted with no tail: park so the reflector's other
                // stop signals are the only thing that can end it.
                std::future::pending::<()>().await;
                unreachable!()
            };
            // `SessionRestarted` is what a real watcher emits before replaying
            // current state; the script does not have to repeat it.
            if tx
                .send(WorkloadObservation::SessionRestarted)
                .await
                .is_err()
            {
                return Ok(());
            }
            for observation in observations {
                if tx.send(observation).await.is_err() {
                    return Ok(());
                }
            }
            outcome
        }
    }

    /// Records what it was asked to do and nothing else. Used ONLY by the
    /// lifecycle test, where durable state is irrelevant — the state tests run
    /// against the real sink and a real database on purpose.
    struct CountingSink {
        applied: AtomicU64,
    }

    #[async_trait::async_trait]
    impl TaskRunAdmissionSink for CountingSink {
        async fn apply(
            &self,
            _task_run_id: &str,
            _admission: &WorkloadAdmission,
            _workload_name: Option<&str>,
        ) -> Result<SinkOutcome, String> {
            self.applied.fetch_add(1, Ordering::SeqCst);
            Ok(SinkOutcome::Unchanged)
        }
        async fn forget(&self, _task_run_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// A project, epic, task and a LIVE task-run row, as dispatch would leave
    /// them once the pod has started.
    async fn live_task_run(db: &Database) -> String {
        let project = crate::test_helpers::create_test_project(db).await;
        let epic = crate::test_helpers::create_test_epic(db, &project.id).await;
        let task = crate::test_helpers::create_test_task(db, &project.id, &epic.id).await;
        let task_run_id = uuid::Uuid::now_v7().to_string();
        TaskRunRepository::new(db.clone())
            .create(djinn_db::CreateTaskRunParams {
                id: &task_run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
                status: Some("running"),
                workspace_path: None,
                mirror_ref: None,
                dispatch_group_id: None,
            })
            .await
            .expect("create task_run");
        task_run_id
    }

    async fn run_status(db: &Database, task_run_id: &str) -> String {
        TaskRunRepository::new(db.clone())
            .get(task_run_id)
            .await
            .expect("read task_run")
            .expect("task_run exists")
            .status
    }

    fn durable_reconciler(db: &Database) -> Arc<Reconciler> {
        Arc::new(Reconciler::new(Arc::new(DurableAdmissionSink::new(
            db.clone(),
        ))))
    }

    // ── AC1 ───────────────────────────────────────────────────────────────

    /// Both directions, against real durable state.
    ///
    /// The admitted half alone is satisfied by a reconciler that marks
    /// everything admitted unconditionally, so the eviction half is the whole
    /// test: a Workload preempted for quota must drive the task-run BACK to
    /// pending — and must take the live run row with it, because a run whose Pod
    /// Kueue deleted is not running any more.
    #[tokio::test]
    async fn admission_drives_a_task_run_out_of_pending_and_eviction_drives_it_back() {
        let db = crate::test_helpers::create_test_db();
        let task_run_id = live_task_run(&db).await;
        let reconciler = durable_reconciler(&db);
        let projection = KueueWorkloadAdmissionRepository::new(db.clone());

        // Queued: waiting on Kueue quota.
        reconciler.handle(queued(&task_run_id)).await;
        assert_eq!(
            projection
                .get(&task_run_id)
                .await
                .expect("read projection")
                .expect("row exists")
                .admission,
            "pending",
            "a Workload Kueue has not admitted must read as pending"
        );

        // Admitted: out of pending, within one observation.
        reconciler.handle(admitted(&task_run_id)).await;
        let after_admission = projection
            .get(&task_run_id)
            .await
            .expect("read projection")
            .expect("row exists");
        assert_eq!(after_admission.admission, "admitted");
        assert_eq!(
            after_admission.transitions, 1,
            "pending -> admitted is exactly one transition"
        );
        assert!(
            projection.pending().await.expect("read pending").is_empty(),
            "an admitted task-run must leave the pending population"
        );
        assert_eq!(
            run_status(&db, &task_run_id).await,
            "running",
            "admission must not disturb the live run"
        );

        // Evicted for quota: back to pending, and the live run is interrupted.
        reconciler.handle(evicted_for_quota(&task_run_id)).await;
        let after_eviction = projection
            .get(&task_run_id)
            .await
            .expect("read projection")
            .expect("row exists");
        assert_eq!(
            after_eviction.admission, "pending",
            "a quota eviction must drive the task-run BACK to pending"
        );
        assert_eq!(
            after_eviction.reason.as_deref(),
            Some("Preempted"),
            "Kueue's own reason is what makes a stalled board explicable"
        );
        assert_eq!(after_eviction.transitions, 2);
        assert_eq!(
            projection.pending().await.expect("read pending").len(),
            1,
            "the evicted task-run must be back in the pending population"
        );
        assert_eq!(
            run_status(&db, &task_run_id).await,
            "interrupted",
            "Kueue deleted the Pod; leaving the run `running` strands it until the generic \
             stall reaper fires"
        );
    }

    // ── AC2 ───────────────────────────────────────────────────────────────

    /// A Workload with no matching task-run is ignored — and "ignored" is
    /// asserted as "the unrelated task-run's state did not move", not as "no
    /// error was returned". A reconciler that resolved every unidentifiable
    /// Workload to some default task-run would return `Ok` all day.
    #[tokio::test]
    async fn an_unmatched_workload_is_ignored_and_mutates_no_unrelated_task_state() {
        let db = crate::test_helpers::create_test_db();
        let unrelated_run = live_task_run(&db).await;
        let reconciler = durable_reconciler(&db);
        let projection = KueueWorkloadAdmissionRepository::new(db.clone());

        // Establish real state for the unrelated run.
        reconciler.handle(admitted(&unrelated_run)).await;
        let before = projection
            .get(&unrelated_run)
            .await
            .expect("read projection")
            .expect("row exists");
        assert_eq!(before.admission, "admitted");
        let transitions_before = reconciler.transitions();

        // A warm Job's Workload, evicted. Same label, same stream, not a
        // task-run.
        reconciler.handle(warm_workload_evicted()).await;

        assert_eq!(
            reconciler.ignored(),
            1,
            "a Workload that resolves to no task-run must be counted as ignored"
        );
        assert_eq!(
            reconciler.transitions(),
            transitions_before,
            "an ignored Workload must apply nothing"
        );
        assert_eq!(
            projection
                .get(&unrelated_run)
                .await
                .expect("read projection")
                .expect("row exists"),
            before,
            "the unrelated task-run's admission state must be byte-identical"
        );
        assert_eq!(
            run_status(&db, &unrelated_run).await,
            "running",
            "an unmatched eviction must not interrupt somebody else's run"
        );
    }

    // ── AC3 ───────────────────────────────────────────────────────────────

    /// Leadership loss drops the reflector task.
    ///
    /// The proof is `Weak::upgrade` returning `None` AFTER the task has ended:
    /// a reflector holding a strong `Arc` would keep the leadership state alive
    /// and never reach that state, while an assertion phrased as "did the loop
    /// stop?" against a strong reference passes either way.
    #[tokio::test]
    async fn releasing_leadership_drops_the_reflector_task() {
        let sink = Arc::new(CountingSink {
            applied: AtomicU64::new(0),
        });
        let reconciler = Arc::new(Reconciler::new(sink));
        let weak = Arc::downgrade(&reconciler);
        // Sessions that deliver one observation and then end, forever: the loop
        // keeps turning and exercises both the per-observation upgrade and the
        // between-session one.
        let watch = ScriptedWatch::new(
            vec![],
            Some((
                vec![admitted(&uuid::Uuid::now_v7().to_string())],
                Ok::<(), String>(()),
            )),
        );
        let cancel = CancellationToken::new();
        let reflector = tokio::spawn(run_reflector(
            weak.clone(),
            watch,
            cancel.clone(),
            Duration::from_millis(1),
        ));

        // Let it establish and apply at least one observation.
        for _ in 0..500 {
            if reconciler.observations() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            reconciler.observations() > 0,
            "the reflector must be running before leadership is released"
        );

        // Leadership released: drop the ONE strong handle. The cancellation
        // token is deliberately NOT fired — this asserts the `Weak` is a real
        // stop signal on its own.
        drop(reconciler);

        let exit = tokio::time::timeout(Duration::from_secs(5), reflector)
            .await
            .expect(
                "a reflector holding a strong Arc never ends: it would keep the leadership \
                     state alive forever",
            )
            .expect("reflector task must not panic");
        assert_eq!(exit, ReflectorExit::LeadershipReleased);
        assert!(
            weak.upgrade().is_none(),
            "the reflector must not have been holding a strong handle"
        );
        assert!(!cancel.is_cancelled(), "the token was never fired");
    }

    /// Leader-only by composition, mirroring `graph_retention`'s gate: the spawn
    /// must appear exactly once in `AppState`, inside `become_leader`, and never
    /// on the every-pod `initialize` path.
    #[test]
    fn spawn_is_composed_only_in_become_leader() {
        let state_source = include_str!("server/state/mod.rs");
        let spawn = "crate::kueue_workload_reconcile::spawn(self.clone())";
        assert_eq!(
            state_source.matches(spawn).count(),
            1,
            "the Workload reflector writes; exactly one pod may run it"
        );
        let leader = state_source
            .find("pub async fn become_leader")
            .expect("become_leader exists");
        let reconcile = state_source.find(spawn).expect("spawn is composed");
        assert!(reconcile > leader);
        let initialize = state_source
            .find("pub async fn initialize(&self)")
            .expect("initialize exists");
        assert!(
            !state_source[initialize..leader].contains(spawn),
            "initialize runs on every pod, including standbys"
        );
    }

    // ── AC4 ───────────────────────────────────────────────────────────────

    /// A watch disconnect re-establishes and replays, and the replay must cost
    /// nothing.
    ///
    /// The assertion is on the transition COUNT — both the reconciler's and the
    /// projection's — not on "did it reconnect". A reflector that reconnects
    /// perfectly and re-applies every replayed Workload as a fresh event
    /// produces a projection whose `transitions` climbs on watch churn alone,
    /// which is exactly the number an operator would use to decide the queue is
    /// thrashing.
    #[tokio::test]
    async fn a_watch_disconnect_resyncs_without_duplicating_transitions() {
        let db = crate::test_helpers::create_test_db();
        let task_run_id = live_task_run(&db).await;
        let reconciler = durable_reconciler(&db);
        let projection = KueueWorkloadAdmissionRepository::new(db.clone());

        let watch = ScriptedWatch::new(
            vec![
                // Session 1: queue, then admit. Then the stream BREAKS.
                (
                    vec![queued(&task_run_id), admitted(&task_run_id)],
                    Err("watch connection reset by peer".to_owned()),
                ),
                // Session 2: the reconnect replays current state verbatim.
                (vec![admitted(&task_run_id)], Ok(())),
            ],
            None,
        );
        let cancel = CancellationToken::new();
        let reflector = tokio::spawn(run_reflector(
            Arc::downgrade(&reconciler),
            watch,
            cancel.clone(),
            Duration::from_millis(1),
        ));

        // Wait for session 1 to have been fully applied.
        for _ in 0..500 {
            if reconciler.transitions() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let transitions_before = reconciler.transitions();
        let sessions_before = reconciler.sessions();
        let projected_before = projection
            .get(&task_run_id)
            .await
            .expect("read projection")
            .expect("row exists");
        assert_eq!(
            transitions_before, 2,
            "record + pending->admitted is two applied changes"
        );
        assert_eq!(projected_before.admission, "admitted");
        assert_eq!(projected_before.transitions, 1);

        // Wait for the resync to be established and its replay consumed.
        for _ in 0..500 {
            if reconciler.sessions() > sessions_before && reconciler.observations() >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            reconciler.sessions() > sessions_before,
            "the broken stream must be re-established"
        );

        cancel.cancel();
        let exit = tokio::time::timeout(Duration::from_secs(5), reflector)
            .await
            .expect("reflector must observe cancellation")
            .expect("reflector task must not panic");
        assert_eq!(exit, ReflectorExit::Cancelled);

        assert_eq!(
            reconciler.transitions(),
            transitions_before,
            "a resync replaying the SAME admission state must apply no further transitions"
        );
        assert_eq!(
            projection
                .get(&task_run_id)
                .await
                .expect("read projection")
                .expect("row exists")
                .transitions,
            projected_before.transitions,
            "the durable transition counter must not move on a watch replay"
        );
        assert_eq!(
            run_status(&db, &task_run_id).await,
            "running",
            "a replayed admission must not touch the run"
        );
    }

    // ── inertness ─────────────────────────────────────────────────────────

    /// The current production shape: Kueue unarmed, no Workloads, no CRDs.
    /// Nothing is watched and nothing is spawned.
    #[tokio::test]
    async fn an_unarmed_cluster_starts_no_watch() {
        // `DJINN_RUNTIME` is unset in tests, so this exercises the first gate;
        // the arming gate is asserted directly on the config default below.
        assert!(
            production_watch().await.is_none(),
            "no Workload watch may be opened outside a Kubernetes runtime"
        );
        assert!(
            !djinn_k8s::KubernetesConfig::from_env().kueue_armed,
            "kueue.armed defaults false; arming is epic 4c9q's cutover step, not this slice's"
        );
    }
}
