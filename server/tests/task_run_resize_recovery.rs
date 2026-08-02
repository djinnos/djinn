// djinn:allow-oversize — task `r37r`'s design names
// `server/tests/task_run_resize_recovery.rs` as the single exclusive path for
// the restart/worker-death matrix, so the durable fixtures cannot move into a
// sibling module without leaving that set.
//! Durable recovery for proposal `3i92`'s resize lifecycle (epic `0ppk`, task
//! `r37r`): the externally owned reconciler, proven against real Postgres.
//!
//! # The property, stated so it cannot be satisfied cheaply
//!
//! `release_lease` is **only ever sent by a worker that reached
//! `queued == true` and survived**. A Pod that dies mid-invocation sends
//! nothing. So the drop cannot belong to the process that performed the lift:
//! if it does, a worker death strands resize responsibility inside the Pod that
//! died, the `build_pod_permits` row never leaves its nonterminal state, and
//! the durable `build_leases` row is held by nobody who will ever release it.
//!
//! Everything below therefore either (a) destroys the in-memory state and
//! rebuilds from a connection string, or (b) never lets the worker's code path
//! run at all.
//!
//! # What is real here and what is faked
//!
//! * **Postgres is real.** [`Database::open_in_memory`] is a template-cloned
//!   real Postgres database, not an in-memory stand-in. There is no `Fake` or
//!   `Mock` permit repository anywhere in this file, deliberately: "resumes from
//!   PERSISTED state" is the whole property, and a fake holding in-memory rows
//!   would assert the exact opposite.
//! * **The restart is real.** [`Restart::restart`] drops every in-memory object
//!   — reconciler, drop bridge, apiserver surface, connection pool — and
//!   rebuilds them from nothing but the database's DSN. Re-invoking a function
//!   on a live object would prove nothing about durability.
//! * **Leadership is real.** The standby proof races two
//!   [`djinn_server::leadership::run_with_leadership`] futures for the same
//!   Postgres advisory lock, in the test's own isolated database.
//! * **The apiserver is faked**, through
//!   [`djinn_k8s::pod_resize_fixture::StoredTaskRunPod`] — a stored `Pod` driven
//!   by the PRODUCTION `PodResizeClient` and the PRODUCTION
//!   `observe_launcher_sidecar`. A confirmation here is confirmed by the same
//!   `status.initContainerStatuses` millicore rule that runs in the cluster.
//!   That is the fault-injection seam, and it is the only fake.
//!
//! # The one criterion this file does NOT prove, and why it cannot
//!
//! Task `r37r`'s acceptance criterion 5 asks for the worker-death scenario
//! against a LIVE kind cluster. It is not here, and this is a structural
//! boundary rather than an omission:
//!
//! * The reconciler lives in `djinn-server`. Driving a live apiserver from a
//!   `djinn-server` test requires a `kube::Client`, and `deny.toml` bans a
//!   direct `k8s-openapi`/`kube` dependency outside `djinn-k8s`, the Kubernetes
//!   capability owner. That ban is right and this slice does not weaken it.
//! * The natural fix — a context-pinned surface constructor on
//!   `djinn_k8s::runtime` so the server test never names `kube` — lands in
//!   `djinn-k8s/src/runtime.rs`, which `r37r`'s design places on the
//!   DO-NOT-TOUCH list.
//! * `djinn-k8s` cannot depend on `djinn-server`, so the test cannot move there
//!   either.
//!
//! What the live half would add over what is here is the KUBELET's actuation.
//! Everything else in that criterion — the 250ms→2s bounded retry inside a 30s
//! budget, quarantine on deadline, the UID-fenced DELETE, and releasing the
//! durable lease only after the exact Pod UID is proven absent — is proven
//! below through the PRODUCTION `PodResizeClient` and the PRODUCTION
//! `confirm_launcher_cpu`, against a stored `Pod`. The "no `release_lease` ever
//! arrives" half is structural here and stronger than an assertion: this file
//! composes no RPC listener and no `DirectServices` at all, so there is nothing
//! a worker could have sent a terminal signal to.
//!
//! # A safety landmine this file does NOT step on
//!
//! Every `KubernetesRuntime` constructor resolves through
//! `kube::Client::try_default()`, which reads the CURRENT kubeconfig context —
//! and every context on a maintainer's machine is a live EKS production
//! cluster. Composing the production Pod plane in a test would silently arm
//! production with no `--context` flag involved and nothing looking wrong.
//! Nothing here constructs `TaskRunPodResizeSurface`; the bridge is always
//! built through `with_surface`, which never resolves an ambient client.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use djinn_coordinator::build_lease::BuildLeaseService;
use djinn_db::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, BuildLeaseConsumerKind, BuildLeaseKey,
    BuildLeaseRepository, BuildLeaseState, BuildPodPermitRepository, BuildPodPermitRow,
    BuildPodPermitState, BuildPodResizeIdentity, CaptureBuildPodResizeIdentityResult,
    CreateTaskRunParams, Database, EffectiveCreatorProvenance, ProjectRepository, TaskRepository,
    TaskRunRepository, TransitionBuildPodResizeLifecycleResult,
};
use djinn_k8s::pod_resize_fixture::{ApiFault, StoredTaskRunPod};
use djinn_k8s::runtime::{JobAdmission, LauncherObservationError, ObservedLauncherSidecar};
use djinn_server::task_run_resize_bootstrap::TaskRunPodSurface;
use djinn_server::task_run_resize_drop::{ResizeDropClock, TaskRunResizeDropBridge};
use djinn_server::task_run_resize_reconcile::{
    DROP_TRANSIT_GRACE, PRE_BIRTH_STRAND_GRACE, ResizeReconcileGate, ResizeReconcileMode,
    SkipReason, TaskRunResizeReconciler, run_loop,
};
use djinn_supervisor::services::{
    LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest,
    LeaseResult, TaskInvocationLeaseIdentity,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const CEILING: &str = "4";
const CEILING_MILLICORES: i64 = 4000;
const LIFTED_MILLICORES: u64 = 4000;
const BIRTH_MILLICORES: u64 = 250;

// ── The apiserver surface, counted ─────────────────────────────────────────

/// [`StoredTaskRunPod`] behind [`TaskRunPodSurface`], counting GETs and DELETEs.
///
/// The PATCH counter lives on the stored Pod itself
/// ([`StoredTaskRunPod::resize_patches`]), which is what makes "exactly one
/// PATCH across two concurrent drivers" an assertion about the cluster rather
/// than about this wrapper.
struct CountingSurface {
    cluster: StoredTaskRunPod,
    gets: AtomicU64,
    deletes_attempted: AtomicU64,
}

impl CountingSurface {
    fn new(cluster: StoredTaskRunPod) -> Self {
        Self {
            cluster,
            gets: AtomicU64::new(0),
            deletes_attempted: AtomicU64::new(0),
        }
    }

    /// Fresh apiserver reads. A drop that cached the observed Pod between
    /// attempts would drop this below the attempt count.
    fn gets(&self) -> u64 {
        self.gets.load(Ordering::SeqCst)
    }

    fn deletes_attempted(&self) -> u64 {
        self.deletes_attempted.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TaskRunPodSurface for CountingSurface {
    async fn observe_launcher(
        &self,
        _task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.cluster.observe_launcher()
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), djinn_k8s::pod_resize::PodResizeError> {
        self.cluster
            .resize_launcher_cpu(pod_name, target_millicores)
            .await
    }

    async fn observe_job_admission(&self, _task_run_id: &str) -> JobAdmission {
        self.cluster.job_admission()
    }

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        self.deletes_attempted.fetch_add(1, Ordering::SeqCst);
        self.cluster.uid_fenced_delete(task_run_id, pod_uid)
    }
}

// ── The injected clock ─────────────────────────────────────────────────────

/// Records what it was asked to wait for and never actually waits.
///
/// The recorded sequence is how the reconciler's timing is compared against
/// `task_run_resize_drop`'s constants without spending 30 seconds of wall clock
/// per test.
struct RecordingClock {
    base: Instant,
    offset: StdMutex<Duration>,
    sleeps: StdMutex<Vec<Duration>>,
    stall_after: Option<usize>,
}

impl RecordingClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: StdMutex::new(Duration::ZERO),
            sleeps: StdMutex::new(Vec::new()),
            stall_after: None,
        }
    }

    fn stalling_after(sleeps: usize) -> Self {
        Self {
            stall_after: Some(sleeps),
            ..Self::new()
        }
    }

    fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().expect("clock").clone()
    }
}

#[async_trait]
impl ResizeDropClock for RecordingClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().expect("clock")
    }

    async fn sleep(&self, duration: Duration) {
        let count = {
            let mut sleeps = self.sleeps.lock().expect("clock");
            sleeps.push(duration);
            *self.offset.lock().expect("clock") += duration;
            sleeps.len()
        };
        if self.stall_after.is_some_and(|limit| count >= limit) {
            std::future::pending::<()>().await;
        }
        tokio::task::yield_now().await;
    }
}

// ── The per-tick gate, under test control ──────────────────────────────────

/// A gate whose mode a test can change while the loop is running, and which
/// counts how many times the loop asked.
struct FlippableGate {
    mode: StdMutex<ResizeReconcileMode>,
    asks: AtomicUsize,
}

impl FlippableGate {
    fn starting(mode: ResizeReconcileMode) -> Self {
        Self {
            mode: StdMutex::new(mode),
            asks: AtomicUsize::new(0),
        }
    }

    fn set(&self, mode: ResizeReconcileMode) {
        *self.mode.lock().expect("gate") = mode;
    }

    fn asks(&self) -> usize {
        self.asks.load(Ordering::SeqCst)
    }
}

impl ResizeReconcileGate for FlippableGate {
    fn mode(&self) -> ResizeReconcileMode {
        self.asks.fetch_add(1, Ordering::SeqCst);
        *self.mode.lock().expect("gate")
    }
}

/// A gate fixed at [`ResizeReconcileMode::Enforce`].
struct ArmedGate;

impl ResizeReconcileGate for ArmedGate {
    fn mode(&self) -> ResizeReconcileMode {
        ResizeReconcileMode::Enforce
    }
}

// ── Durable seeding, against real Postgres ─────────────────────────────────

async fn seed_task_run(db: &Database, suffix: &str) -> (String, String) {
    let user = djinn_db::UserRepository::new(db.clone())
        .upsert_from_github(
            i64::try_from(uuid::Uuid::now_v7().as_u128() % 8_000_000_000_000_000_000)
                .expect("github id"),
            &format!("resize-recover-{suffix}-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed user");
    let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create(
            &format!("resize-recover-{suffix}"),
            "djinnos",
            &format!("resize-recover-{suffix}"),
        )
        .await
        .expect("seed project");
    let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_in_project_with_provenance(
            &project.id,
            None,
            EffectiveCreatorProvenance::explicit_user_id(&user.id),
            "resize recovery",
            "description",
            "design",
            "task",
            2,
            "owner",
            None,
            None,
        )
        .await
        .expect("seed task");
    let task_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed task run");
    (task.id, task_run_id)
}

/// Kill the worker, as the durable record sees it.
///
/// This is the ONLY signal the tests below ever give that a worker has died,
/// and it is written by the coordinator's own reconciliation rather than by the
/// worker: a Pod that was OOM-killed cannot send anything, which is the whole
/// premise. No `release_lease` is ever sent anywhere in this file.
async fn kill_the_worker(db: &Database, task_run_id: &str) {
    TaskRunRepository::new(db.clone())
        .update_status(task_run_id, djinn_core::models::TaskRunStatus::Failed)
        .await
        .expect("terminalise the task run");
}

async fn captured_permit(
    permits: &BuildPodPermitRepository,
    task_run_id: &str,
    pod_uid: &str,
) -> BuildPodPermitRow {
    let row = match permits.acquire(task_run_id, 16).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("unexpected acquire outcome: {other:?}"),
    };
    let bound = match permits
        .bind_or_refresh_job_uid(
            task_run_id,
            &row.permit_id,
            row.fencing_token,
            &format!("job-{pod_uid}"),
        )
        .await
        .expect("bind job uid")
    {
        BindBuildPodPermitResult::Bound(row) => row,
        other => panic!("unexpected bind outcome: {other:?}"),
    };
    let identity = BuildPodResizeIdentity {
        pod_namespace: "djinn".to_owned(),
        pod_name: format!("taskrun-{pod_uid}"),
        pod_uid: pod_uid.to_owned(),
        launcher_container_name: "cgroup-launcher".to_owned(),
        launcher_container_id: "containerd://launcher".to_owned(),
        image_digest: "registry/img@sha256:feed".to_owned(),
        observed_launcher_protocol: "resize-v2".to_owned(),
        effective_launcher_protocol: "resize-v2".to_owned(),
        admitted_cpu_millicores: CEILING_MILLICORES,
    };
    match permits
        .capture_resize_identity(
            task_run_id,
            &bound.permit_id,
            bound.fencing_token,
            &identity,
        )
        .await
        .expect("capture identity")
    {
        CaptureBuildPodResizeIdentityResult::Captured(row) => row,
        other => panic!("unexpected capture outcome: {other:?}"),
    }
}

/// Drive the row and the Pod through a REAL lift, then park it in `state`.
///
/// NON-VACUITY: this asserts the launcher's own init-container status reports
/// the lifted ceiling before any resumption is asked to bring it back down, so
/// a "drop confirmed" below can never be a Pod that was never raised.
async fn lift_then_park(
    permits: &BuildPodPermitRepository,
    cluster: &StoredTaskRunPod,
    row: &BuildPodPermitRow,
    pod_uid: &str,
    invocation_id: &str,
    state: BuildPodPermitState,
) {
    if state == BuildPodPermitState::BirthConfirmed {
        return;
    }
    match permits
        .begin_resize_invocation(
            &row.task_run_id,
            &row.permit_id,
            row.fencing_token,
            pod_uid,
            invocation_id,
        )
        .await
        .expect("begin invocation")
    {
        TransitionBuildPodResizeLifecycleResult::Transitioned(_) => {}
        other => panic!("unexpected claim outcome: {other:?}"),
    }
    cluster
        .resize_launcher_cpu(&format!("taskrun-{pod_uid}"), LIFTED_MILLICORES)
        .await
        .expect("lift the launcher");
    assert_eq!(
        status_millicores(cluster),
        Some(LIFTED_MILLICORES),
        "NON-VACUITY: the launcher must actually be lifted before recovery is \
         asked to bring it back down"
    );
    if state == BuildPodPermitState::LiftApplying {
        return;
    }
    let mut current = BuildPodPermitState::LiftApplying;
    for next in [
        BuildPodPermitState::Lifted,
        BuildPodPermitState::DropRequired,
        BuildPodPermitState::DropApplying,
        BuildPodPermitState::Quarantined,
    ] {
        match permits
            .transition_resize_lifecycle(
                &row.task_run_id,
                &row.permit_id,
                row.fencing_token,
                pod_uid,
                Some(invocation_id),
                current,
                next,
            )
            .await
            .expect("park the row")
        {
            TransitionBuildPodResizeLifecycleResult::Transitioned(_) => {}
            other => panic!("unexpected {current:?} -> {next:?} outcome: {other:?}"),
        }
        current = next;
        if current == state {
            return;
        }
    }
    panic!("cannot park a row at {state:?}");
}

/// The launcher's **init-container** status limit, in millicores.
///
/// Millicores, never the Quantity string: the apiserver canonicalises `4000m`
/// to `4`.
fn status_millicores(cluster: &StoredTaskRunPod) -> Option<u64> {
    cluster.launcher_status_cpu().map(|raw| {
        djinn_k8s::pod_resize::CpuLimit::parse(&raw)
            .expect("the launcher reports a parseable quantity")
            .millis()
    })
}

async fn permit_state(db: &Database, task_run_id: &str) -> BuildPodPermitState {
    BuildPodPermitRepository::new(db.clone())
        .active(task_run_id)
        .await
        .expect("read permit")
        .map_or(BuildPodPermitState::Released, |row| row.state)
}

// ── The other ledger ───────────────────────────────────────────────────────

/// Take a real, occupying `build_leases` row for one invocation.
///
/// Returned so a test can prove the lease is still HELD, which is a statement
/// about `build_leases` and cannot be made by reading `build_pod_permits`.
async fn hold_build_lease(
    leases: &BuildLeaseService,
    task_id: &str,
    task_run_id: &str,
    invocation_id: &str,
) -> LeaseFencingToken {
    let identity = LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
        task_id: task_id.to_owned(),
        task_run_id: task_run_id.to_owned(),
        invocation_id: invocation_id.to_owned(),
    });
    let token = match leases
        .queue(LeaseQueueRequest {
            identity: identity.clone(),
            deadlines: LeaseDeadlines {
                queue_deadline_ms: 0,
                launch_deadline_ms: 0,
            },
        })
        .await
    {
        LeaseResult::Granted(grant) => grant.fencing_token,
        other => panic!("expected a grant, got {other:?}"),
    };
    match leases
        .grant(LeaseGrantRequest {
            identity,
            fencing_token: token.clone(),
        })
        .await
    {
        LeaseResult::Status(_) => {}
        other => panic!("expected the grant acknowledgement, got {other:?}"),
    }
    token
}

async fn build_lease_state(db: &Database, invocation_id: &str) -> Option<BuildLeaseState> {
    BuildLeaseRepository::new(db.clone())
        .get(&BuildLeaseKey {
            consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
            consumer_id: invocation_id.to_owned(),
        })
        .await
        .expect("read build_leases")
        .map(|row| row.state)
}

async fn armed_lease_service(db: &Database) -> Arc<BuildLeaseService> {
    let service = Arc::new(BuildLeaseService::new(
        Arc::new(BuildLeaseRepository::new(db.clone())),
        8,
    ));
    assert!(matches!(service.recover().await, LeaseResult::Status(_)));
    assert!(matches!(service.set_cap(8).await, LeaseResult::Status(_)));
    service
}

// ── The restart ────────────────────────────────────────────────────────────

/// A durable database whose in-memory attachments can be thrown away.
///
/// [`Self::owner`] is retained ONLY so the template-cloned test database is not
/// DROPped when the last handle goes; nothing reads through it after a restart.
/// [`Self::restart`] opens a brand-new pool from the DSN and builds a brand-new
/// reconciler, drop bridge and apiserver surface — the objects a lost process
/// takes with it.
struct Restart {
    owner: Database,
    dsn: String,
}

impl Restart {
    async fn new() -> Self {
        let owner = Database::ephemeral().await.expect("ephemeral db");
        let dsn = owner
            .test_dsn()
            .expect("the ephemeral harness must expose its DSN");
        Self { owner, dsn }
    }

    fn db(&self) -> &Database {
        &self.owner
    }

    /// Rebuild everything a restarted process would have to rebuild.
    fn restart(&self, cluster: StoredTaskRunPod, clock: Arc<RecordingClock>) -> Rebuilt {
        let db =
            Database::reopen_test(&self.dsn).expect("reopen the same database from its DSN alone");
        let surface = Arc::new(CountingSurface::new(cluster));
        Rebuilt {
            bridge: Arc::new(TaskRunResizeDropBridge::with_surface(
                db.clone(),
                Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>,
                Arc::clone(&clock) as Arc<dyn ResizeDropClock>,
            )),
            db,
            surface,
        }
    }

    /// Restart onto a surface that races the ledger. See
    /// [`LedgerRacingSurface`].
    fn restart_racing_the_ledger(
        &self,
        cluster: StoredTaskRunPod,
        permit: &BuildPodPermitRow,
    ) -> Arc<TaskRunResizeDropBridge> {
        let db = Database::reopen_test(&self.dsn).expect("reopen from the DSN");
        let surface = Arc::new(LedgerRacingSurface {
            cluster,
            permits: BuildPodPermitRepository::new(db.clone()),
            task_run_id: permit.task_run_id.clone(),
            permit_id: permit.permit_id.clone(),
            fencing_token: permit.fencing_token,
            observations: AtomicU64::new(0),
        });
        Arc::new(TaskRunResizeDropBridge::with_surface(
            db,
            surface as Arc<dyn TaskRunPodSurface>,
            Arc::new(RecordingClock::new()) as Arc<dyn ResizeDropClock>,
        ))
    }
}

/// A surface that moves the durable ledger out from under a drop, between its
/// PATCH and its settle.
///
/// The launcher REALLY comes back to 250m and the cluster REALLY reports it.
/// But the permit row is retired by somebody else in the same instant, so
/// `settle_confirmed`'s compare-and-swap is rejected and the drop settles as
/// [`ResizeDropOutcome::Unavailable`] — "the Pod came back down but the ledger
/// did not move".
///
/// This is the ONLY shape that reaches the reconciler's `releases_lease()`
/// guard at all: a resumption that runs out of its row budget returns `None` and
/// takes the `unsettled` arm long before it. An earlier version of this file
/// claimed the apiserver-unavailability test covered that guard; it did not, and
/// deleting the guard left the whole suite green.
struct LedgerRacingSurface {
    cluster: StoredTaskRunPod,
    permits: BuildPodPermitRepository,
    task_run_id: String,
    permit_id: String,
    fencing_token: i64,
    observations: AtomicU64,
}

#[async_trait]
impl TaskRunPodSurface for LedgerRacingSurface {
    async fn observe_launcher(
        &self,
        _task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        // Observation 1 is the fencing read before the PATCH; observation 2 is
        // the CONFIRMING read the settle immediately follows. Racing the ledger
        // at 2 is what lands the rejection on the settle rather than on the
        // entry transition.
        if self.observations.fetch_add(1, Ordering::SeqCst) == 1 {
            self.permits
                .release(
                    &self.task_run_id,
                    &self.permit_id,
                    self.fencing_token,
                    "test_ledger_race",
                )
                .await
                .expect("retire the permit under the drop");
        }
        self.cluster.observe_launcher()
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), djinn_k8s::pod_resize::PodResizeError> {
        self.cluster
            .resize_launcher_cpu(pod_name, target_millicores)
            .await
    }

    async fn observe_job_admission(&self, _task_run_id: &str) -> JobAdmission {
        self.cluster.job_admission()
    }

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        self.cluster.uid_fenced_delete(task_run_id, pod_uid)
    }
}

struct Rebuilt {
    db: Database,
    surface: Arc<CountingSurface>,
    bridge: Arc<TaskRunResizeDropBridge>,
}

impl Rebuilt {
    fn reconciler(
        &self,
        leases: Option<Arc<BuildLeaseService>>,
        gate: Arc<dyn ResizeReconcileGate>,
    ) -> Arc<TaskRunResizeReconciler> {
        Arc::new(
            TaskRunResizeReconciler::new(self.db.clone(), Arc::clone(&self.bridge), leases, gate)
                .with_row_budget(Duration::from_secs(5)),
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3 — restart resumes from persisted state, in every nonterminal state
// ═══════════════════════════════════════════════════════════════════════════

/// Every nonterminal resize state resumes after the process that lifted the Pod
/// is gone, and a TERMINAL row is skipped.
///
/// The seeding never constructs a reconciler: the row is written by the
/// repository directly, then every in-memory object is dropped and rebuilt from
/// the DSN. The reconciler therefore has no possible source for what to do
/// except `list_nonterminal_resize`.
///
/// NAMED FAILING MUTATION: seed the resumption from an in-memory cache instead
/// of `list_nonterminal_resize` and EVERY row of this table fails — a rebuilt
/// process has no cache to read.
///
/// NON-VACUITY: the seventh case is a released (terminal) row, and it must be
/// left exactly where it is. A scan that acted on everything would move it.
#[tokio::test]
async fn every_nonterminal_state_resumes_after_the_process_that_lifted_it_is_gone() {
    for (index, state) in [
        BuildPodPermitState::BirthConfirmed,
        BuildPodPermitState::LiftApplying,
        BuildPodPermitState::Lifted,
        BuildPodPermitState::DropRequired,
        BuildPodPermitState::DropApplying,
        BuildPodPermitState::Quarantined,
    ]
    .into_iter()
    .enumerate()
    {
        let restart = Restart::new().await;
        let db = restart.db().clone();
        let suffix = format!("state-{index}");
        let (_task_id, task_run_id) = seed_task_run(&db, &suffix).await;
        let pod_uid = format!("pod-uid-{suffix}");
        let invocation_id = format!("invocation-{suffix}");
        let cluster = StoredTaskRunPod::resize_v2(&pod_uid, CEILING);
        {
            // The process that performed the lift. Everything in this block is
            // dropped before the reconciler exists.
            let permits = BuildPodPermitRepository::new(db.clone());
            let row = captured_permit(&permits, &task_run_id, &pod_uid).await;
            lift_then_park(&permits, &cluster, &row, &pod_uid, &invocation_id, state).await;
        }
        kill_the_worker(&db, &task_run_id).await;
        assert_eq!(
            permit_state(&db, &task_run_id).await,
            state,
            "precondition: the durable row is parked at {state:?}"
        );

        cluster.reset_patch_counter();
        let clock = Arc::new(RecordingClock::new());
        let rebuilt = restart.restart(cluster.clone(), Arc::clone(&clock));
        let reconciler = rebuilt.reconciler(None, Arc::new(ArmedGate));

        let pass = reconciler.run_pass().await;

        assert_eq!(
            pass.resumed, 1,
            "{state:?}: the rebuilt process must resume this row from persisted \
             state; pass was {pass:?}"
        );
        let settled = permit_state(&db, &task_run_id).await;
        assert_eq!(
            settled,
            BuildPodPermitState::Released,
            "{state:?}: a permit whose task run has ended must be RETIRED, or the \
             next sweep re-drops the same dead run every tick forever"
        );
        // THE POD IS NEVER RETURNED TO SERVICE: whatever happened, no launcher
        // is left holding a lifted ceiling.
        match status_millicores(&cluster) {
            Some(millicores) => assert_eq!(
                millicores, BIRTH_MILLICORES,
                "{state:?}: the launcher must be back at its birth limit, \
                 confirmed through status.initContainerStatuses in MILLICORES"
            ),
            None => assert!(
                cluster.observe_launcher().expect("observe").is_none(),
                "{state:?}: a launcher with no status limit must be a Pod that \
                 no longer exists"
            ),
        }
        assert_ne!(
            settled,
            BuildPodPermitState::Lifted,
            "{state:?}: the Pod must never be returned to service"
        );
        drop(rebuilt);
    }
}

/// NON-VACUITY for the table above: a terminal row is invisible to the scan.
///
/// NAMED FAILING MUTATION: widen `NONTERMINAL_RESIZE_STATES` (or scan the whole
/// table) and `scanned` becomes 1 and the released row is driven.
#[tokio::test]
async fn a_terminal_row_is_not_scanned_and_not_acted_on() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "terminal").await;
    let pod_uid = "pod-uid-terminal";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    permits
        .release(
            &task_run_id,
            &row.permit_id,
            row.fencing_token,
            "test_terminal",
        )
        .await
        .expect("release the permit");
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released,
        "precondition: the row is terminal"
    );
    kill_the_worker(&db, &task_run_id).await;

    cluster.reset_patch_counter();
    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let pass = rebuilt
        .reconciler(None, Arc::new(ArmedGate))
        .run_pass()
        .await;

    assert_eq!(
        pass.scanned, 0,
        "a terminal row must not even be returned by the scan; pass was {pass:?}"
    );
    assert_eq!(pass.resumed, 0);
    assert_eq!(
        cluster.resize_patches(),
        0,
        "no PATCH may be issued for a released permit"
    );
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released
    );
}

/// A LIVE build is not touched.
///
/// `lifted` is the normal state of a healthy invocation. A reconciler that
/// resumed every nonterminal row it found would take the CPU away from every
/// running build in the cluster on its first tick — the failure mode that makes
/// this whole slice dangerous rather than merely inert.
///
/// NAMED FAILING MUTATION: make `is_stranded` return `Ok(true)` unconditionally
/// and this test fails with the launcher back at 250m while its build is still
/// running.
#[tokio::test]
async fn a_live_build_keeps_its_lift() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "live").await;
    let pod_uid = "pod-uid-live";
    let invocation_id = "invocation-live";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    // The worker is ALIVE: the task run is still `running` and the exact Pod is
    // still present.
    cluster.reset_patch_counter();

    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let pass = rebuilt
        .reconciler(None, Arc::new(ArmedGate))
        .run_pass()
        .await;

    assert_eq!(pass.scanned, 1, "the row IS in the scan; pass was {pass:?}");
    assert_eq!(
        pass.resumed, 0,
        "a live build must keep its lift; pass was {pass:?}"
    );
    assert_eq!(
        pass.skipped,
        vec![(task_run_id.clone(), SkipReason::OwnerLive)]
    );
    assert_eq!(
        cluster.resize_patches(),
        0,
        "not one PATCH may be issued against a live build"
    );
    assert_eq!(
        status_millicores(&cluster),
        Some(LIFTED_MILLICORES),
        "the live launcher keeps the ceiling its invocation was granted"
    );
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Lifted
    );
}

/// A live task run whose Pod is GONE is still stranded.
///
/// The task-run status is written by the coordinator and can lag a node that
/// disappeared; the exact Pod UID being absent is a second, independent
/// worker-independent fact, and the ceiling is withheld for the whole of that
/// window.
///
/// NAMED FAILING MUTATION: delete the `pod_uid_absent` disjunct from
/// `is_stranded` and this row is never resumed.
#[tokio::test]
async fn a_lifted_row_whose_pod_vanished_is_stranded_even_while_the_run_reads_live() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "vanished").await;
    let pod_uid = "pod-uid-vanished";
    let invocation_id = "invocation-vanished";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    // The node took the Pod with it. The task run still reads `running`.
    cluster
        .uid_fenced_delete(&task_run_id, pod_uid)
        .expect("delete");
    assert!(
        cluster.observe_launcher().expect("observe").is_none(),
        "precondition: the Pod is gone"
    );
    assert_eq!(
        TaskRunRepository::new(db.clone())
            .get(&task_run_id)
            .await
            .expect("read run")
            .expect("run exists")
            .status,
        "running",
        "precondition: nothing has terminalised the task run"
    );

    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let pass = rebuilt
        .reconciler(None, Arc::new(ArmedGate))
        .run_pass()
        .await;

    assert_eq!(
        pass.resumed, 1,
        "an absent Pod UID strands the row on its own; pass was {pass:?}"
    );
    assert_eq!(
        pass.permits_retired, 1,
        "`PodUidAbsent` leaves the row nonterminal inside the drop and hands it \
         to THIS module to retire; pass was {pass:?}"
    );
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released,
        "the write-once identity means this task run can never capture a \
         replacement Pod, so the permit is dead weight nothing else will retire"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// The drop states a LIVE worker transits
// ═══════════════════════════════════════════════════════════════════════════

/// A drop a living worker is in the middle of performing is NOT the
/// reconciler's.
///
/// This is the 2026-08-01 production defect. `require_drop` writes
/// `lifted -> drop_required` and `apply_drop` writes
/// `drop_required -> drop_applying`, both from inside a `release_lease` that is
/// still running; the row then sits at `drop_applying` for about a second while
/// the kubelet actuates. A 30-second sweep landing in that window used to
/// classify the row `DropOwed` unconditionally and start a second drop against
/// the same Pod. Both drivers then raced `drop_applying -> birth_confirmed`,
/// the loser settled `Unavailable`, and `release_lease` answered
/// `LeaseUnavailable` for a drop that had succeeded — three of which force a
/// cancel. It happened in 6 of 12 resumptions in one session.
///
/// The task run is `running` and the exact Pod is present, which is everything
/// the previous predicate had to work with. What separates this row from a
/// genuinely abandoned one is only its AGE, and the sibling test below is the
/// half that proves the age is a window rather than a blanket refusal.
///
/// NAMED FAILING MUTATION: restore
/// `DropRequired | DropApplying | Quarantined => Ok(Strandedness::DropOwed)` in
/// `strandedness`. Both rows are then resumed, `resumed` reads 1, a PATCH is
/// issued against a Pod whose own worker is mid-drop, and this test fails.
///
/// NAMED FAILING MUTATION 2: compare the age against `Duration::ZERO` instead
/// of `self.drop_transit_grace`. Every row is then instantly stale and this
/// test fails identically, which is what keeps the grace from being tuned to
/// nothing while the arm that reads it stays in place.
///
/// NAMED FAILING MUTATION 3: delete
/// `IF NEW.state IS DISTINCT FROM OLD.state THEN NEW.state_changed_at := now();`
/// from migration 170's trigger. The permit row below is deliberately aged two
/// hours BEFORE the lift, so without the stamp the age never returns to zero
/// and the precondition assertion fails. That is the shape in which this fix
/// would otherwise ship inert: every real build's permit row is far older than
/// any grace, so a column that is only ever set at INSERT grants no grace at
/// all.
#[tokio::test]
async fn a_drop_a_live_worker_is_still_performing_is_not_stolen() {
    for (index, state) in [
        BuildPodPermitState::DropRequired,
        BuildPodPermitState::DropApplying,
    ]
    .into_iter()
    .enumerate()
    {
        let restart = Restart::new().await;
        let db = restart.db().clone();
        let suffix = format!("mid-drop-{index}");
        let (_task_id, task_run_id) = seed_task_run(&db, &suffix).await;
        let pod_uid = format!("pod-uid-{suffix}");
        let invocation_id = format!("invocation-{suffix}");
        let cluster = StoredTaskRunPod::resize_v2(&pod_uid, CEILING);
        let permits = BuildPodPermitRepository::new(db.clone());
        let row = captured_permit(&permits, &task_run_id, &pod_uid).await;
        // A LONG-RUNNING task run: the permit row itself is two hours old.
        // Backdating BEFORE the lift is what makes this test a statement about
        // the STATE CHANGE rather than about row age — a grace keyed to when
        // the permit was acquired would be permanently expired for every real
        // build, which is the fix quietly doing nothing.
        permits
            .backdate_state_change_for_test(&task_run_id, Duration::from_secs(7200))
            .await
            .expect("age the permit row");
        lift_then_park(&permits, &cluster, &row, &pod_uid, &invocation_id, state).await;

        // The worker is ALIVE and its drop is IN FLIGHT: the task run is still
        // `running`, the exact Pod is still present, and the row reached this
        // state moments ago.
        assert!(
            permits
                .active(&task_run_id)
                .await
                .expect("read permit")
                .expect("permit exists")
                .state_age()
                < Duration::from_secs(5),
            "{state:?}: precondition — the row must be freshly moved, as a live \
             worker's own drop leaves it"
        );
        cluster.reset_patch_counter();

        let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
        let pass = rebuilt
            .reconciler(None, Arc::new(ArmedGate))
            .run_pass()
            .await;

        assert_eq!(
            pass.scanned, 1,
            "{state:?}: the row IS in the scan; pass was {pass:?}"
        );
        assert_eq!(
            pass.resumed, 0,
            "{state:?}: a drop a live worker is performing must not be resumed; \
             pass was {pass:?}"
        );
        assert_eq!(
            pass.skipped,
            vec![(task_run_id.clone(), SkipReason::OwnerLive)],
            "{state:?}: the row belongs to a live driver"
        );
        assert_eq!(
            cluster.resize_patches(),
            0,
            "{state:?}: not one PATCH may be issued against a Pod whose own \
             worker is mid-drop"
        );
        // THE DURABLE ROW, not a log line: the lifecycle is exactly where the
        // worker left it, so the worker's own `drop_applying -> birth_confirmed`
        // compare-and-swap will still succeed and its `release_lease` will not
        // answer `LeaseUnavailable`.
        assert_eq!(
            permit_state(&db, &task_run_id).await,
            state,
            "{state:?}: the reconciler must leave the row for its owner"
        );
        assert_eq!(
            status_millicores(&cluster),
            Some(LIFTED_MILLICORES),
            "{state:?}: the reconciler must not have actuated a downsize the \
             worker is already actuating"
        );
        drop(rebuilt);
    }
}

/// NON-VACUITY for the test above: the grace is a WINDOW, and a drop nobody is
/// performing any more is still resumed.
///
/// This is the case neither `owner_is_gone` nor `pod_uid_absent` can see: the
/// server restarted mid-drop while the task run kept running and its Pod stayed
/// up. Nothing will ever emit a terminal signal for the row, so without the age
/// it would hold its lifted ceiling and its `build_leases` row for the rest of
/// the run. The reconciler must still be the thing that ends that.
///
/// NAMED FAILING MUTATION: delete the
/// `if row.state_age() >= self.drop_transit_grace` arm from `strandedness`, so
/// the drop states are graced whenever their owner reads live. `resumed` reads
/// 0, the row stays at `drop_applying` and the launcher stays at 4000m — the
/// reconciler would have been disabled for exactly the rows it exists for.
#[tokio::test]
async fn a_drop_abandoned_past_the_grace_is_resumed_even_while_the_run_reads_live() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "abandoned-drop").await;
    let pod_uid = "pod-uid-abandoned-drop";
    let invocation_id = "invocation-abandoned-drop";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::DropApplying,
    )
    .await;

    // Age the row past the production grace. Everything else is identical to
    // the test above: the run reads `running`, the Pod is present.
    permits
        .backdate_state_change_for_test(&task_run_id, DROP_TRANSIT_GRACE + Duration::from_secs(30))
        .await
        .expect("age the row");
    assert_eq!(
        TaskRunRepository::new(db.clone())
            .get(&task_run_id)
            .await
            .expect("read run")
            .expect("run exists")
            .status,
        "running",
        "precondition: nothing has terminalised the task run"
    );
    assert!(
        cluster.observe_launcher().expect("observe").is_some(),
        "precondition: the Pod is still present, so `pod_uid_absent` cannot be \
         what strands this row"
    );
    cluster.reset_patch_counter();

    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let pass = rebuilt
        .reconciler(None, Arc::new(ArmedGate))
        .run_pass()
        .await;

    assert_eq!(
        pass.resumed, 1,
        "a drop older than any live driver could hold it must be resumed; pass \
         was {pass:?}"
    );
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::BirthConfirmed,
        "the resumed drop must settle the row back at its birth state"
    );
    assert_eq!(
        status_millicores(&cluster),
        Some(BIRTH_MILLICORES),
        "the CPU must actually come back, confirmed through \
         status.initContainerStatuses in MILLICORES"
    );
    // The run is still LIVE, so its permit is not dead weight and must survive.
    // Retiring it here would mean the task run could never lift again.
    assert_eq!(
        pass.permits_retired, 0,
        "a live task run's permit must not be retired; pass was {pass:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC2 — the gate is on the ACTION, evaluated per tick
// ═══════════════════════════════════════════════════════════════════════════

/// A disarmed reconciler is a loop that RUNS and declines to act, and arming it
/// takes effect on the next tick without a restart.
///
/// NAMED FAILING MUTATION: move the gate to wrap the `spawn(...)` call — i.e.
/// decide at wiring time — and this test cannot even be written: there would be
/// no loop to observe ticking, `gate.asks()` would stay at 0 or 1, and the row
/// would never be picked up after the flip.
///
/// NAMED FAILING MUTATION 2: read the mode once before the loop and cache it,
/// and the second half fails with the row still stranded.
#[tokio::test]
async fn the_gate_is_evaluated_every_tick_and_arming_needs_no_restart() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "gate").await;
    let pod_uid = "pod-uid-gate";
    let invocation_id = "invocation-gate";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    kill_the_worker(&db, &task_run_id).await;
    cluster.reset_patch_counter();

    let gate = Arc::new(FlippableGate::starting(ResizeReconcileMode::Off));
    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let reconciler = rebuilt.reconciler(None, Arc::clone(&gate) as Arc<dyn ResizeReconcileGate>);

    let cancel = CancellationToken::new();
    let loop_reconciler = Arc::clone(&reconciler);
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_loop(Duration::from_millis(20), loop_cancel, move || {
            let reconciler = Arc::clone(&loop_reconciler);
            async move {
                reconciler.run_pass().await;
            }
        })
        .await;
    });

    // ── Disarmed: the loop is RUNNING and idle. ──
    wait_until(|| {
        let reconciler = Arc::clone(&reconciler);
        async move { reconciler.passes() >= 4 }
    })
    .await;
    assert!(
        gate.asks() >= 4,
        "the loop must ask the gate on every tick; asked {} times",
        gate.asks()
    );
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Lifted,
        "a disarmed tick must not move the durable row"
    );
    assert_eq!(
        cluster.resize_patches(),
        0,
        "a disarmed tick must not touch the cluster"
    );

    // ── Armed mid-flight. No restart, no respawn. ──
    gate.set(ResizeReconcileMode::Enforce);
    wait_until(|| {
        let cluster = cluster.clone();
        async move { cluster.resize_patches() > 0 }
    })
    .await;

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released,
        "arming is a configuration change, not a restart: the SAME loop must \
         pick the stranded row up on a later tick"
    );
    assert_eq!(status_millicores(&cluster), Some(BIRTH_MILLICORES));
}

/// `observe` classifies but never acts.
///
/// NAMED FAILING MUTATION: make `acts()` return true for `Observe` and the
/// PATCH counter leaves zero.
#[tokio::test]
async fn observe_mode_classifies_without_touching_the_cluster() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "observe").await;
    let pod_uid = "pod-uid-observe";
    let invocation_id = "invocation-observe";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    kill_the_worker(&db, &task_run_id).await;
    cluster.reset_patch_counter();

    let gate = Arc::new(FlippableGate::starting(ResizeReconcileMode::Observe));
    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let pass = rebuilt
        .reconciler(None, gate as Arc<dyn ResizeReconcileGate>)
        .run_pass()
        .await;

    assert_eq!(pass.would_resume, 1, "pass was {pass:?}");
    assert_eq!(pass.resumed, 0);
    assert_eq!(cluster.resize_patches(), 0);
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Lifted,
        "observe must leave the durable row exactly where it found it"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC4 — the sweep is periodic, not startup-only
// ═══════════════════════════════════════════════════════════════════════════

/// A row that becomes stranded while the reconciler is ALREADY RUNNING is
/// picked up on a later tick, with no restart.
///
/// This is the criterion that exists because this repository shipped a
/// startup-only reaper paired with a read-only doctor and produced a
/// phantom-active state that never self-healed.
///
/// NAMED FAILING MUTATION: make the scan run once at startup and return, and
/// this test fails by leaving the row stranded forever — the row does not even
/// exist when the loop starts.
#[tokio::test]
async fn a_row_stranded_while_the_loop_runs_is_picked_up_on_a_later_tick() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "midflight").await;
    let pod_uid = "pod-uid-midflight";
    let invocation_id = "invocation-midflight";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);

    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let reconciler = rebuilt.reconciler(None, Arc::new(ArmedGate));
    let cancel = CancellationToken::new();
    let loop_reconciler = Arc::clone(&reconciler);
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        run_loop(Duration::from_millis(20), loop_cancel, move || {
            let reconciler = Arc::clone(&loop_reconciler);
            async move {
                reconciler.run_pass().await;
            }
        })
        .await;
    });

    // The loop has already done its startup scan over an EMPTY table.
    wait_until(|| {
        let reconciler = Arc::clone(&reconciler);
        async move { reconciler.passes() >= 3 }
    })
    .await;

    // Only now does a build start, lift, and lose its worker.
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    kill_the_worker(&db, &task_run_id).await;
    cluster.reset_patch_counter();

    wait_until(|| {
        let db = db.clone();
        let task_run_id = task_run_id.clone();
        async move { permit_state(&db, &task_run_id).await == BuildPodPermitState::Released }
    })
    .await;

    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    assert_eq!(
        status_millicores(&cluster),
        Some(BIRTH_MILLICORES),
        "the launcher a dead worker left lifted must come back down without any \
         restart of the reconciler"
    );
    assert!(cluster.resize_patches() >= 1);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC6 — release is gated on the OTHER ledger
// ═══════════════════════════════════════════════════════════════════════════

/// `build_leases` moves only after the CLUSTER confirmed the drop.
///
/// Both ledgers are read. Asserting `build_pod_permits.state` alone would be a
/// success log from the wrong store.
///
/// NAMED FAILING MUTATION: release the lease whenever the pass resumed a row
/// (rather than on `releases_lease()`), and the unavailability test below fails.
#[tokio::test]
async fn the_build_lease_is_released_only_after_the_cluster_confirms_the_drop() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (task_id, task_run_id) = seed_task_run(&db, "ledger").await;
    let pod_uid = "pod-uid-ledger";
    let invocation_id = "invocation-ledger";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;

    let leases = armed_lease_service(&db).await;
    hold_build_lease(&leases, &task_id, &task_run_id, invocation_id).await;
    assert_eq!(
        build_lease_state(&db, invocation_id).await,
        Some(BuildLeaseState::Launching),
        "precondition: the OTHER ledger holds an occupying row"
    );

    // The worker dies without ever sending `release_lease`.
    kill_the_worker(&db, &task_run_id).await;
    cluster.reset_patch_counter();

    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let pass = rebuilt
        .reconciler(Some(Arc::clone(&leases)), Arc::new(ArmedGate))
        .run_pass()
        .await;

    assert_eq!(pass.resumed, 1, "pass was {pass:?}");
    assert_eq!(pass.leases_released, 1, "pass was {pass:?}");
    assert_eq!(
        status_millicores(&cluster),
        Some(BIRTH_MILLICORES),
        "the confirmation that authorised the release came from the launcher's \
         init-container status, in millicores"
    );
    assert_eq!(
        pass.permits_retired, 1,
        "the permit ledger moves FIRST; pass was {pass:?}"
    );
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released
    );
    assert_eq!(
        build_lease_state(&db, invocation_id).await,
        Some(BuildLeaseState::Terminal),
        "THE OTHER LEDGER: `build_leases` is what capacity is counted from, and \
         it must be released by the reconciler for a worker that never sent \
         `release_lease`"
    );
}

/// An unreachable apiserver holds the lease and keeps the row nonterminal,
/// retrying forever.
///
/// NAMED FAILING MUTATION: release the lease when the row reaches `quarantined`
/// and this test fails with `build_leases` terminal while a Pod still holds a
/// lifted ceiling nobody is accounting for.
#[tokio::test]
async fn an_unreachable_apiserver_holds_the_lease_and_keeps_retrying() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (task_id, task_run_id) = seed_task_run(&db, "unreachable").await;
    let pod_uid = "pod-uid-unreachable";
    let invocation_id = "invocation-unreachable";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;

    let leases = armed_lease_service(&db).await;
    hold_build_lease(&leases, &task_id, &task_run_id, invocation_id).await;
    kill_the_worker(&db, &task_run_id).await;

    // The apiserver stops answering entirely. Nothing can be confirmed and
    // nothing can be proven absent.
    cluster.fail_gets(ApiFault::timeout());
    cluster.fail_patches(ApiFault::timeout());
    cluster.reset_patch_counter();

    // The clock stalls after enough sleeps to prove the loop is UNBOUNDED: if
    // the quarantine absence loop had a cap it would have returned by now.
    const STALL_AFTER: usize = 40;
    let clock = Arc::new(RecordingClock::stalling_after(STALL_AFTER));
    let rebuilt = restart.restart(cluster.clone(), Arc::clone(&clock));
    let reconciler = rebuilt.reconciler(Some(Arc::clone(&leases)), Arc::new(ArmedGate));

    let pass = tokio::time::timeout(Duration::from_secs(30), reconciler.run_pass())
        .await
        .expect("the pass must return once its per-row budget expires");

    assert!(
        clock.sleeps().len() >= STALL_AFTER,
        "the drop must still have been retrying when the pass gave up; only \
         {} sleeps observed",
        clock.sleeps().len()
    );
    assert_eq!(
        pass.unsettled, 1,
        "the resumption did not settle; pass was {pass:?}"
    );
    assert_eq!(
        pass.leases_released, 0,
        "an unanswered apiserver is not evidence that CPU came back"
    );
    assert_eq!(
        build_lease_state(&db, invocation_id).await,
        Some(BuildLeaseState::Launching),
        "THE OTHER LEDGER: `build_leases` must still be HELD"
    );
    let settled = permit_state(&db, &task_run_id).await;
    assert!(
        matches!(
            settled,
            BuildPodPermitState::DropApplying | BuildPodPermitState::Quarantined
        ),
        "the permit row stays nonterminal so the next sweep picks it up, got \
         {settled:?}"
    );
}

/// A SETTLED outcome that is not releasable still holds the lease.
///
/// The launcher is confirmed back at 250m — the cluster said so, through
/// `status.initContainerStatuses` — and the lease is STILL not released,
/// because the durable lifecycle did not move with it. Holding is the only
/// answer that keeps the two ledgers in agreement.
///
/// This is the test that guards `outcome.releases_lease() &&` in `run_pass`.
/// It exists because the apiserver-unavailability test above does NOT: an
/// unsettled resumption returns `None` and takes the `unsettled` arm before the
/// release site is ever evaluated, so deleting that condition left the entire
/// suite green. The named mutation was verified against a real deletion.
///
/// NAMED FAILING MUTATION: drop `outcome.releases_lease() &&` from the release
/// condition in `run_pass` and this fails with `build_leases` terminal.
#[tokio::test]
async fn a_settled_but_unreleasable_outcome_still_holds_the_lease() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (task_id, task_run_id) = seed_task_run(&db, "unreleasable").await;
    let pod_uid = "pod-uid-unreleasable";
    let invocation_id = "invocation-unreleasable";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::DropApplying,
    )
    .await;
    let leases = armed_lease_service(&db).await;
    hold_build_lease(&leases, &task_id, &task_run_id, invocation_id).await;
    kill_the_worker(&db, &task_run_id).await;
    cluster.reset_patch_counter();

    let bridge = restart.restart_racing_the_ledger(cluster.clone(), &row);
    let reconciler = Arc::new(
        TaskRunResizeReconciler::new(
            Database::reopen_test(&restart.dsn).expect("reopen"),
            bridge,
            Some(Arc::clone(&leases)),
            Arc::new(ArmedGate),
        )
        .with_row_budget(Duration::from_secs(5)),
    );
    let pass = reconciler.run_pass().await;

    assert_eq!(pass.resumed, 1, "pass was {pass:?}");
    assert_eq!(
        pass.unsettled, 0,
        "NON-VACUITY: this must be a SETTLED outcome, not one that ran out of \
         budget — the budget path returns before the release site; pass was {pass:?}"
    );
    assert_eq!(
        status_millicores(&cluster),
        Some(BIRTH_MILLICORES),
        "NON-VACUITY: the cluster really did bring the launcher back down, so \
         'the lease is held' is not held because nothing happened"
    );
    assert_eq!(
        pass.leases_released, 0,
        "an outcome the drop refused to call releasable must not release the \
         durable lease; pass was {pass:?}"
    );
    assert_eq!(
        build_lease_state(&db, invocation_id).await,
        Some(BuildLeaseState::Launching),
        "THE OTHER LEDGER: `build_leases` must still be HELD"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC7 — concurrent ownership is fenced
// ═══════════════════════════════════════════════════════════════════════════

/// Two concurrent drivers over one stranded row issue exactly ONE PATCH.
///
/// The reconciler has no prior claim on a row resting in a lift state: it must
/// WIN the compare-and-swap into `drop_required` before it may touch the Pod.
///
/// NAMED FAILING MUTATION: delete the `claim` call (drive `drop_to_birth`
/// directly, as the worker's own terminal path does) and this fails with a
/// doubled PATCH counter — both drivers actuate the same drop.
#[tokio::test]
async fn two_concurrent_drivers_issue_exactly_one_patch() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "concurrent").await;
    let pod_uid = "pod-uid-concurrent";
    let invocation_id = "invocation-concurrent";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    kill_the_worker(&db, &task_run_id).await;
    cluster.reset_patch_counter();

    // TWO SEPARATE PROCESSES: separate pools, separate reconcilers, separate
    // in-flight sets. Only the durable compare-and-swap can serialise them.
    let left = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let right = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let left_reconciler = left.reconciler(None, Arc::new(ArmedGate));
    let right_reconciler = right.reconciler(None, Arc::new(ArmedGate));

    let (left_pass, right_pass) =
        tokio::join!(left_reconciler.run_pass(), right_reconciler.run_pass(),);

    let resumed = left_pass.resumed + right_pass.resumed;
    assert_eq!(
        resumed, 1,
        "exactly one driver may claim the row; left={left_pass:?} right={right_pass:?}"
    );
    assert!(
        left_pass
            .skipped
            .iter()
            .chain(right_pass.skipped.iter())
            .any(|(_, reason)| *reason == SkipReason::ClaimLost),
        "the losing driver must be REJECTED, not merely idle; \
         left={left_pass:?} right={right_pass:?}"
    );
    assert_eq!(
        cluster.resize_patches(),
        1,
        "exactly one pods/resize PATCH per drop, across both drivers"
    );
    assert_eq!(status_millicores(&cluster), Some(BIRTH_MILLICORES));
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released
    );
}

/// A leadership handover mid-drop: the new leader resumes and the OLD leader's
/// in-flight attempt cannot commit.
///
/// NAMED FAILING MUTATION: drop the invocation/Pod-UID fence from
/// `transition_resize_lifecycle` (or settle on the row's current state without
/// a compare-and-swap) and the stalled old leader commits a second settlement
/// on top of the new leader's, releasing the lease twice.
#[tokio::test]
async fn a_leadership_handover_mid_drop_lets_only_the_new_leader_commit() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (task_id, task_run_id) = seed_task_run(&db, "handover").await;
    let pod_uid = "pod-uid-handover";
    let invocation_id = "invocation-handover";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::DropApplying,
    )
    .await;
    let leases = armed_lease_service(&db).await;
    hold_build_lease(&leases, &task_id, &task_run_id, invocation_id).await;
    kill_the_worker(&db, &task_run_id).await;

    // THE OLD LEADER, mid-drop and stuck. Its view of the cluster never
    // actuates — which is exactly WHY it is still in flight — so it is given its
    // own stored Pod carrying the same UID. The DURABLE row is shared, and the
    // durable row is the thing under test.
    let stalled_view = StoredTaskRunPod::resize_v2(pod_uid, CEILING).never_actuating();
    let old_clock = Arc::new(RecordingClock::stalling_after(2));
    let old = restart.restart(stalled_view, Arc::clone(&old_clock));
    let old_reconciler = old.reconciler(Some(Arc::clone(&leases)), Arc::new(ArmedGate));
    let old_leader = tokio::spawn(async move { old_reconciler.run_pass().await });
    wait_until(|| {
        let clock = Arc::clone(&old_clock);
        async move { clock.sleeps().len() >= 2 }
    })
    .await;
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::DropApplying,
        "precondition: the old leader is mid-drop and has committed nothing"
    );

    // THE NEW LEADER, on a cluster that answers. Same durable row.
    cluster.reset_patch_counter();
    let new = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let new_pass = new
        .reconciler(Some(Arc::clone(&leases)), Arc::new(ArmedGate))
        .run_pass()
        .await;

    assert_eq!(new_pass.resumed, 1, "pass was {new_pass:?}");
    assert_eq!(new_pass.leases_released, 1, "pass was {new_pass:?}");
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released
    );
    assert_eq!(
        build_lease_state(&db, invocation_id).await,
        Some(BuildLeaseState::Terminal)
    );

    // The old leader is still parked, holding the exact fences it entered with.
    assert!(
        !old_leader.is_finished(),
        "precondition: the old leader really is still in flight"
    );

    // ITS IN-FLIGHT COMMIT CANNOT LAND. This is the write the stalled driver
    // would issue the moment its Pod finally reported 250m: the settle out of
    // `drop_applying`, fenced on the same permit, token, Pod UID and invocation.
    let rejected = permits
        .transition_resize_lifecycle(
            &task_run_id,
            &row.permit_id,
            row.fencing_token,
            pod_uid,
            Some(invocation_id),
            BuildPodPermitState::DropApplying,
            BuildPodPermitState::BirthConfirmed,
        )
        .await
        .expect("the compare-and-swap must answer");
    assert_eq!(
        rejected,
        TransitionBuildPodResizeLifecycleResult::Rejected,
        "the old leader's in-flight attempt must be REJECTED by the durable \
         compare-and-swap; without the fence it would commit a second settlement \
         on top of the new leader's and release the lease twice"
    );

    old_leader.abort();
}

// ═══════════════════════════════════════════════════════════════════════════
// AC8 — no duplicated machinery
// ═══════════════════════════════════════════════════════════════════════════

/// The reconciler's observed backoff IS `task_run_resize_drop`'s schedule.
///
/// The literals are spelled out rather than referenced through the constants:
/// asserting `sleeps == [DROP_INITIAL_BACKOFF, ...]` would keep passing after
/// someone changed them, which is the opposite of what this criterion wants.
///
/// NAMED FAILING MUTATION: change `DROP_INITIAL_BACKOFF`, `DROP_MAX_BACKOFF` or
/// `DROP_CONFIRMATION_BUDGET` in `task_run_resize_drop.rs` alone and BOTH this
/// test and `0ppk-2`'s
/// `the_backoff_sequence_is_250ms_doubling_capped_at_2s_inside_30s` fail
/// together. If only one fails, the schedule has been forked.
#[tokio::test]
async fn the_reconcilers_backoff_is_the_drops_backoff() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "backoff").await;
    let pod_uid = "pod-uid-backoff";
    let invocation_id = "invocation-backoff";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING).never_actuating();
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    kill_the_worker(&db, &task_run_id).await;

    let clock = Arc::new(RecordingClock::new());
    let rebuilt = restart.restart(cluster.clone(), Arc::clone(&clock));
    let reconciler = rebuilt
        .reconciler(None, Arc::new(ArmedGate))
        .run_pass()
        .await;
    assert_eq!(reconciler.resumed, 1, "pass was {reconciler:?}");

    // 250 + 500 + 1000 + 2000*n <= 30_000, with the deadline checked against
    // the sleep the loop is ABOUT to take.
    let mut expected = vec![
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_millis(1000),
    ];
    expected.extend(std::iter::repeat_n(Duration::from_secs(2), 14));
    let sleeps = clock.sleeps();
    assert_eq!(
        &sleeps[..expected.len()],
        &expected[..],
        "the reconciler must observe `task_run_resize_drop`'s schedule, not a \
         second copy of it"
    );
    assert!(
        rebuilt.surface.gets() as usize >= sleeps.len(),
        "a FRESH GET before EVERY attempt: {} gets for {} sleeps",
        rebuilt.surface.gets(),
        sleeps.len()
    );
}

/// A quarantined Pod is destroyed with a UID-FENCED delete and the permit is
/// released only after its absence is OBSERVED.
///
/// A `200` from `DELETE` says the apiserver accepted the request, not that the
/// object is gone.
///
/// NAMED FAILING MUTATION: settle the quarantine on the delete's own
/// acknowledgement rather than on a subsequent observation, and the assertion
/// that the Pod is actually gone fails.
#[tokio::test]
async fn a_quarantined_pod_is_uid_fence_deleted_and_proven_absent() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (task_id, task_run_id) = seed_task_run(&db, "quarantine").await;
    let pod_uid = "pod-uid-quarantine";
    let invocation_id = "invocation-quarantine";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Quarantined,
    )
    .await;
    let leases = armed_lease_service(&db).await;
    hold_build_lease(&leases, &task_id, &task_run_id, invocation_id).await;
    kill_the_worker(&db, &task_run_id).await;

    let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
    let pass = rebuilt
        .reconciler(Some(Arc::clone(&leases)), Arc::new(ArmedGate))
        .run_pass()
        .await;

    assert_eq!(pass.resumed, 1, "pass was {pass:?}");
    assert!(
        rebuilt.surface.deletes_attempted() >= 1,
        "the quarantined Pod must be destroyed"
    );
    assert_eq!(
        cluster.deletes(),
        vec![(task_run_id.clone(), pod_uid.to_owned())],
        "the DELETE must be fenced to the EXACT Pod UID"
    );
    assert!(
        cluster.observe_launcher().expect("observe").is_none(),
        "absence is proven by observation, never by the DELETE's own answer"
    );
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released
    );
    assert_eq!(
        build_lease_state(&db, invocation_id).await,
        Some(BuildLeaseState::Terminal),
        "THE OTHER LEDGER: only an ABSENT Pod UID may release build_leases here"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1 — reachability: leadership, and the composition site
// ═══════════════════════════════════════════════════════════════════════════

/// The spawn lives in `become_leader` and NOWHERE in the every-pod
/// `initialize` path.
///
/// `become_leader` cannot be invoked from a test — it exits the process on a
/// Zot retention preflight failure and starts the coordinator, the RPC listener
/// and the image controller — so the composition site is asserted the way this
/// file's neighbours already assert theirs: against the source text, plus the
/// standby proof below against real leadership, plus
/// `scripts/check-resize-reachability.sh`, which fails when the symbol has no
/// production caller at all.
///
/// NAMED FAILING MUTATION: delete the spawn line and this test fails.
#[test]
fn the_reconciler_is_spawned_from_become_leader_and_not_from_initialize() {
    let source = include_str!("../src/server/state/mod.rs");
    // Assembled at runtime so this assertion cannot match its own literal.
    let call = format!(
        "crate::task_run_resize_{}::spawn(self.clone());",
        "reconcile"
    );
    assert_eq!(
        source.matches(call.as_str()).count(),
        1,
        "exactly one production call site"
    );
    let initialize = source
        .find("pub async fn initialize(&self)")
        .expect("initialize exists");
    let leader = source
        .find("pub async fn become_leader")
        .expect("become_leader exists");
    let site = source.find(call.as_str()).expect("call site exists");
    assert!(
        site > leader,
        "the reconciler must be armed inside become_leader"
    );
    assert!(
        !source[initialize..leader].contains(call.as_str()),
        "a standby pod running drop reconciliation would race the leader"
    );
}

/// A standby pod never reconciles, proven through the REAL advisory-lock
/// leadership transition.
///
/// Two processes race `run_with_leadership` for the coordinator lock in this
/// test's own isolated database. The winner's `on_acquire` runs a pass; the
/// loser's never fires at all, so its reconciler never sees the stranded row.
///
/// NAMED FAILING MUTATION: move the spawn into `initialize` (which every pod
/// runs) and the property this asserts — that only the lock holder acts —
/// stops being true of production, though this test would keep passing;
/// `the_reconciler_is_spawned_from_become_leader_and_not_from_initialize` above
/// is the assertion that catches that move, and the two are deliberately paired.
/// Multi-threaded on purpose: this is the only test here that runs two
/// long-lived background processes AND a polling loop at the same time, and on
/// the default current-thread runtime they all contend for one thread. Under
/// `--test-threads 4` that showed up as a 30-second timeout on a test that takes
/// three seconds alone, which is a flake, not a finding.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn only_the_leader_reconciles_across_a_real_advisory_lock_race() {
    let restart = Restart::new().await;
    let db = restart.db().clone();
    let (_task_id, task_run_id) = seed_task_run(&db, "standby").await;
    let pod_uid = "pod-uid-standby";
    let invocation_id = "invocation-standby";
    let cluster = StoredTaskRunPod::resize_v2(pod_uid, CEILING);
    let permits = BuildPodPermitRepository::new(db.clone());
    let row = captured_permit(&permits, &task_run_id, pod_uid).await;
    lift_then_park(
        &permits,
        &cluster,
        &row,
        pod_uid,
        invocation_id,
        BuildPodPermitState::Lifted,
    )
    .await;
    kill_the_worker(&db, &task_run_id).await;
    cluster.reset_patch_counter();

    let dsn = restart.db().test_dsn().expect("dsn");
    let acquired = Arc::new(AtomicUsize::new(0));
    let cancel = CancellationToken::new();

    let mut pods = Vec::new();
    for _ in 0..2 {
        let rebuilt = restart.restart(cluster.clone(), Arc::new(RecordingClock::new()));
        let reconciler = rebuilt.reconciler(None, Arc::new(ArmedGate));
        let acquired = Arc::clone(&acquired);
        let dsn = dsn.clone();
        let cancel = cancel.clone();
        pods.push(tokio::spawn(async move {
            let _rebuilt = rebuilt;
            let loop_cancel = cancel.clone();
            djinn_server::leadership::run_with_leadership(Some(dsn), cancel, || async move {
                acquired.fetch_add(1, Ordering::SeqCst);
                // The PRODUCTION shape: a loop, not a single pass. A one-shot
                // here would make the test hostage to any transient database
                // hiccup — one failed scan and the row is stranded for the rest
                // of the test — which is exactly the startup-only failure mode
                // this whole slice exists to avoid.
                run_loop(Duration::from_millis(50), loop_cancel, move || {
                    let reconciler = Arc::clone(&reconciler);
                    async move {
                        reconciler.run_pass().await;
                    }
                })
                .await;
            })
            .await;
        }));
    }

    // `Released`, not `BirthConfirmed`: the pass drives the row all the way
    // through the birth limit to permit retirement, so polling for the
    // intermediate state is a race the poller usually loses.
    wait_until(|| {
        let db = db.clone();
        let task_run_id = task_run_id.clone();
        async move { permit_state(&db, &task_run_id).await == BuildPodPermitState::Released }
    })
    .await;
    // Give the standby ample opportunity to promote itself if it were going to.
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(
        acquired.load(Ordering::SeqCst),
        1,
        "exactly one process may hold the coordinator advisory lock, and only \
         that one may reconcile"
    );
    assert_eq!(
        cluster.resize_patches(),
        1,
        "the standby must not have touched the Pod"
    );

    cancel.cancel();
    for pod in pods {
        let _ = tokio::time::timeout(Duration::from_secs(10), pod).await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
// The pre-birth strand: permits that never reached the resize lifecycle
// ═══════════════════════════════════════════════════════════════════════════
//
// `list_nonterminal_resize` scans the six migration-164 states, every one of
// them PAST the birth capture. A permit refused before `capture_resize_identity`
// ever succeeded rests at `acquired` or `job_created` with `pod_uid IS NULL` and
// is invisible to that scan forever. Production on 2026-08-02 held 70 such rows
// — 58 `job_created`, 12 `acquired`, the oldest ~10 hours — against three live
// task-run Jobs, and nothing in the system could see any of them.

/// A reconciler for the pre-birth tests.
///
/// The drop bridge it carries is deliberately never exercised: a pre-birth row
/// has no `pod_uid`, so there is no Pod to PATCH and the reaper must reach its
/// decision without one. A test in which the bridge did work would be measuring
/// the lifecycle path instead of the gap.
fn pre_birth_reconciler(db: &Database) -> Arc<TaskRunResizeReconciler> {
    let surface = Arc::new(CountingSurface::new(StoredTaskRunPod::resize_v2(
        "pod-uid-never-observed",
        "4",
    )));
    let bridge = Arc::new(TaskRunResizeDropBridge::with_surface(
        db.clone(),
        surface as Arc<dyn TaskRunPodSurface>,
        Arc::new(RecordingClock::new()) as Arc<dyn ResizeDropClock>,
    ));
    Arc::new(TaskRunResizeReconciler::new(
        db.clone(),
        bridge,
        None,
        Arc::new(ArmedGate),
    ))
}

/// Seed a permit that reached `job_created` and never captured an identity —
/// the exact production shape, `pod_uid IS NULL` included.
async fn pre_birth_permit(
    permits: &BuildPodPermitRepository,
    task_run_id: &str,
) -> BuildPodPermitRow {
    let row = match permits.acquire(task_run_id, 16).await {
        AcquireBuildPodPermitResult::Acquired { row, .. } => row,
        other => panic!("unexpected acquire outcome: {other:?}"),
    };
    let bound = match permits
        .bind_or_refresh_job_uid(
            task_run_id,
            &row.permit_id,
            row.fencing_token,
            "job-uid-pre-birth",
        )
        .await
        .expect("bind job uid")
    {
        BindBuildPodPermitResult::Bound(row) => row,
        other => panic!("unexpected bind outcome: {other:?}"),
    };
    assert_eq!(bound.state, BuildPodPermitState::JobCreated);
    assert!(
        bound.resize_identity.is_none(),
        "the fixture must model a permit that never captured an identity"
    );
    bound
}

/// A permit stranded before the birth capture, whose task run has ended, is
/// released — and the capacity it was holding comes back.
///
/// **Named mutation.** Delete the `self.reap_pre_birth(&mut pass, mode).await;`
/// call from `TaskRunResizeReconciler::run_pass` (or the `permits.release(..)`
/// call inside `reap_pre_birth`). This test then fails on the row state, which
/// stays `job_created` forever — which is precisely what production did for ten
/// hours. Note the assertion is on the ROW, not on `pass.pre_birth_reaped`: a
/// counter can be incremented by a body that does nothing.
#[tokio::test]
async fn a_permit_stranded_before_the_birth_capture_is_released_once_its_owner_is_gone() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let (_task_id, task_run_id) = seed_task_run(&db, "prebirth-reap").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    let seeded = pre_birth_permit(&permits, &task_run_id).await;

    // The two facts the reaper requires, both worker-independent.
    kill_the_worker(&db, &task_run_id).await;
    permits
        .backdate_state_change_for_test(
            &task_run_id,
            PRE_BIRTH_STRAND_GRACE + Duration::from_secs(60),
        )
        .await
        .expect("backdate the strand");

    // The premise: this row is invisible to the lifecycle scan the reconciler
    // has always had. If this ever stops holding, the reaper is redundant and
    // this whole path should be reconsidered rather than kept.
    assert!(
        permits
            .list_nonterminal_resize()
            .await
            .expect("nonterminal scan")
            .iter()
            .all(|row| row.task_run_id != task_run_id),
        "a pre-birth permit must NOT be reachable through list_nonterminal_resize; \
         if it is, this test is no longer measuring the gap it was written for"
    );

    let pass = pre_birth_reconciler(&db).run_pass().await;

    assert_eq!(pass.pre_birth_reaped, 1, "one row was reaped");
    assert!(!pass.pre_birth_scan_failed);

    // THE STATE THE MECHANISM PRODUCED, read back from Postgres.
    assert_eq!(
        permit_state(&db, &task_run_id).await,
        BuildPodPermitState::Released,
        "the stranded permit must be released, not left at job_created"
    );

    // And the consequence that actually matters: the capacity it was holding is
    // back. This is the number production had wrong — 70 permits occupying the
    // pool against three live Jobs.
    assert_eq!(
        permits.active_count().await.expect("active count"),
        0,
        "a released permit must stop occupying build-pod capacity"
    );

    // The row is not resurrectable under the same task run, so the reap is
    // final rather than a state a later actor can undo.
    assert!(
        matches!(
            permits.acquire(&task_run_id, 16).await,
            AcquireBuildPodPermitResult::AlreadyReleased { .. }
        ),
        "a reaped permit must stay released; the seeded token was {}",
        seeded.fencing_token
    );
}

/// The reaper is a WINDOW, not a blanket. A permit that is equally old but whose
/// task run is still live belongs to the dispatch path, and is left alone.
///
/// Without this, "reap old pre-birth permits" would retire the permit of every
/// run sitting in a long Kueue queue — the failure mode that turns a leak fix
/// into an outage.
///
/// **Named mutation.** In `reap_pre_birth`, drop the `owner_is_gone` guard (make
/// the `Ok(false)` arm fall through instead of `continue`). This test then fails:
/// the live run's permit comes back `released`.
#[tokio::test]
async fn a_pre_birth_permit_whose_task_run_is_still_live_is_never_reaped() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let (_task_id, task_run_id) = seed_task_run(&db, "prebirth-live").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    pre_birth_permit(&permits, &task_run_id).await;

    // Old enough to be scanned — and deliberately NOT terminalised.
    permits
        .backdate_state_change_for_test(
            &task_run_id,
            PRE_BIRTH_STRAND_GRACE + Duration::from_secs(60),
        )
        .await
        .expect("backdate the strand");

    let pass = pre_birth_reconciler(&db).run_pass().await;

    assert_eq!(
        pass.pre_birth_scanned, 1,
        "the row must actually have been scanned, or this test proves nothing about the guard"
    );
    assert_eq!(pass.pre_birth_reaped, 0);
    assert_eq!(
        permits
            .active(&task_run_id)
            .await
            .expect("read permit")
            .expect("permit row")
            .state,
        BuildPodPermitState::JobCreated,
        "a live task run keeps its permit however old the row is"
    );
}

/// A pre-birth permit younger than the grace is not even returned by the scan,
/// whatever its owner's status.
///
/// The grace is wider than the whole dispatch birth window
/// (`QUEUE_ADMISSION_BUDGET_DEFAULT + BIRTH_ADMISSION_BUDGET_DEFAULT`), so a
/// bootstrap still legitimately waiting for a late kubelet cannot have its
/// permit retired out from under it.
///
/// **Named mutation.** Remove the `state_changed_at <= now() - ...` predicate
/// from `list_pre_birth_stranded`. This test then fails on `pre_birth_scanned`.
#[tokio::test]
async fn a_recent_pre_birth_permit_is_below_the_grace_and_is_not_scanned() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let (_task_id, task_run_id) = seed_task_run(&db, "prebirth-young").await;
    let permits = BuildPodPermitRepository::new(db.clone());
    pre_birth_permit(&permits, &task_run_id).await;
    // Terminal owner, so the ONLY thing standing between this row and the
    // reaper is its age.
    kill_the_worker(&db, &task_run_id).await;

    let pass = pre_birth_reconciler(&db).run_pass().await;

    assert_eq!(
        pass.pre_birth_scanned, 0,
        "a permit inside the dispatch birth window must not be scanned at all"
    );
    assert_eq!(pass.pre_birth_reaped, 0);
    assert_eq!(
        permits
            .active(&task_run_id)
            .await
            .expect("read permit")
            .expect("permit row")
            .state,
        BuildPodPermitState::JobCreated
    );
}

/// Poll `condition` until it holds, or fail the test after 30 seconds.
async fn wait_until<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if condition().await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition never became true within 60s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
