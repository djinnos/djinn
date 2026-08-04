//! The fault-injection matrix for proposal `3i92`'s fail-safe drop, driven
//! through the **composition** rather than through the mechanism.
//!
//! `task_run_resize_drop_tests.rs` proves what the drop mechanism does. This
//! file proves the two things a unit test of the mechanism structurally cannot:
//!
//! 1. **The two-ledger ordering.** `BuildLeaseService::release` writes
//!    `build_leases`; the drop lifecycle lives in `build_pod_permits`. DIFFERENT
//!    STORES. A test that asserted only "the permit row moved" would be a
//!    success log from the wrong ledger. Every assertion here reads both.
//! 2. **That `DirectServices::release_lease` actually consults the gate.** The
//!    handler is the composition site, and a mechanism nobody calls is the
//!    failure mode this neighbourhood was burned by seven times in one day.
//!
//! Real Postgres throughout (`Database::ephemeral` is a template-cloned server).
//! The apiserver is `djinn_k8s::pod_resize_fixture::StoredTaskRunPod`, driven
//! through the production `PodResizeClient` and the production
//! `observe_launcher_sidecar`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use djinn_agent::direct_services::DirectServices;
use djinn_coordinator::build_lease::{BuildLeaseService, LeaseClock};
use djinn_db::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, BuildLeaseKey, BuildLeaseRepository,
    BuildLeaseState, BuildPodPermitRepository, BuildPodPermitState, BuildPodResizeIdentity,
    CaptureBuildPodResizeIdentityResult, CreateTaskRunParams, Database, EffectiveCreatorProvenance,
    ProjectRepository, TaskRepository, TaskRunRepository, TransitionBuildPodResizeLifecycleResult,
    UserRepository,
};
use djinn_k8s::pod_resize::PodResizeError;
use djinn_k8s::pod_resize_fixture::{ApiFault, StoredTaskRunPod};
use djinn_k8s::runtime::{JobAdmission, LauncherObservationError, ObservedLauncherSidecar};
use djinn_server::task_run_resize_bootstrap::TaskRunPodSurface;
use djinn_server::task_run_resize_drop::{ResizeDropClock, TaskRunResizeDropBridge};
use djinn_server::task_run_resize_reconcile::{
    ResizeReconcileGate, ResizeReconcileMode, ResizeReconcilePass, SkipReason,
    TaskRunResizeReconciler,
};
use djinn_supervisor::services::{
    LeaseFencingToken, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest, LeaseReleaseRequest,
    LeaseResult, SupervisorServices, TaskInvocationLeaseIdentity,
};
use tokio_util::sync::CancellationToken;

const CEILING: &str = "4";
const CEILING_MILLICORES: i64 = 4000;
const LIFTED_MILLICORES: u64 = 4000;
const BUILD_SLOT_CAP: i64 = 8;

// ── The apiserver surface ──────────────────────────────────────────────────

struct FixtureSurface(StoredTaskRunPod);

#[async_trait]
impl TaskRunPodSurface for FixtureSurface {
    async fn observe_launcher(
        &self,
        _task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        self.0.observe_launcher()
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError> {
        self.0
            .resize_launcher_cpu(pod_name, target_millicores)
            .await
    }

    async fn observe_job_admission(&self, _task_run_id: &str) -> JobAdmission {
        self.0.job_admission()
    }

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        self.0.uid_fenced_delete(task_run_id, pod_uid)
    }
}

/// A clock that never waits and, after `stall_after` sleeps, never returns.
///
/// The stall is what keeps the unbounded quarantine loop inside the gate's own
/// wait budget without the test spending 45 real seconds on it.
struct StallingClock {
    base: tokio::time::Instant,
    offset: std::sync::Mutex<Duration>,
    sleeps: std::sync::Mutex<usize>,
    stall_after: usize,
}

impl StallingClock {
    fn new(stall_after: usize) -> Self {
        Self {
            base: tokio::time::Instant::now(),
            offset: std::sync::Mutex::new(Duration::ZERO),
            sleeps: std::sync::Mutex::new(0),
            stall_after,
        }
    }
}

#[async_trait]
impl ResizeDropClock for StallingClock {
    fn now(&self) -> tokio::time::Instant {
        self.base + *self.offset.lock().expect("clock")
    }

    async fn sleep(&self, duration: Duration) {
        let count = {
            let mut sleeps = self.sleeps.lock().expect("clock");
            *sleeps += 1;
            *self.offset.lock().expect("clock") += duration;
            *sleeps
        };
        if count >= self.stall_after {
            std::future::pending::<()>().await;
        }
        tokio::task::yield_now().await;
    }
}

// ── Durable fixtures ───────────────────────────────────────────────────────

struct Ledgers {
    db: Database,
    permits: Arc<BuildPodPermitRepository>,
    leases: Arc<BuildLeaseRepository>,
    task_id: String,
    task_run_id: String,
    invocation_id: String,
    pod_uid: String,
    cluster: StoredTaskRunPod,
}

impl Ledgers {
    async fn lifted(suffix: &str) -> Self {
        let db = Database::ephemeral().await.expect("ephemeral db");
        let user = UserRepository::new(db.clone())
            .upsert_from_github(
                i64::try_from(uuid::Uuid::now_v7().as_u128() % 8_000_000_000_000_000_000)
                    .expect("github id"),
                &format!("faults-{suffix}-{}", uuid::Uuid::now_v7()),
                None,
                None,
            )
            .await
            .expect("seed user");
        let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .create(
                &format!("faults-{suffix}"),
                "djinnos",
                &format!("faults-{suffix}"),
            )
            .await
            .expect("seed project");
        let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .create_in_project_with_provenance(
                &project.id,
                None,
                EffectiveCreatorProvenance::explicit_user_id(&user.id),
                "resize faults",
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

        let pod_uid = format!("pod-uid-{suffix}");
        let invocation_id = uuid::Uuid::now_v7().to_string();
        let permits = Arc::new(BuildPodPermitRepository::new(db.clone()));
        let row = match permits.acquire(&task_run_id, 16).await {
            AcquireBuildPodPermitResult::Acquired { row, .. } => row,
            other => panic!("unexpected acquire: {other:?}"),
        };
        let bound = match permits
            .bind_or_refresh_job_uid(
                &task_run_id,
                &row.permit_id,
                row.fencing_token,
                &format!("job-{pod_uid}"),
            )
            .await
            .expect("bind job")
        {
            BindBuildPodPermitResult::Bound(row) => row,
            other => panic!("unexpected bind: {other:?}"),
        };
        let identity = BuildPodResizeIdentity {
            pod_namespace: "djinn".to_owned(),
            pod_name: format!("taskrun-{pod_uid}"),
            pod_uid: pod_uid.clone(),
            launcher_container_name: "cgroup-launcher".to_owned(),
            launcher_container_id: "containerd://launcher".to_owned(),
            image_digest: "registry/img@sha256:feed".to_owned(),
            observed_launcher_protocol: "resize-v2".to_owned(),
            effective_launcher_protocol: "resize-v2".to_owned(),
            admitted_cpu_millicores: CEILING_MILLICORES,
        };
        match permits
            .capture_resize_identity(
                &task_run_id,
                &bound.permit_id,
                bound.fencing_token,
                &identity,
            )
            .await
            .expect("capture")
        {
            CaptureBuildPodResizeIdentityResult::Captured(_) => {}
            other => panic!("unexpected capture: {other:?}"),
        }

        // A REAL lift, confirmed through the production client.
        let cluster = StoredTaskRunPod::resize_v2(&pod_uid, CEILING);
        permits
            .begin_resize_invocation(
                &task_run_id,
                &bound.permit_id,
                bound.fencing_token,
                &pod_uid,
                &invocation_id,
            )
            .await
            .expect("claim invocation");
        cluster
            .resize_launcher_cpu(&format!("taskrun-{pod_uid}"), LIFTED_MILLICORES)
            .await
            .expect("lift");
        assert_eq!(
            status_millicores(&cluster),
            Some(LIFTED_MILLICORES),
            "NON-VACUITY: the launcher must actually be lifted"
        );
        match permits
            .transition_resize_lifecycle(
                &task_run_id,
                &bound.permit_id,
                bound.fencing_token,
                &pod_uid,
                Some(&invocation_id),
                BuildPodPermitState::LiftApplying,
                BuildPodPermitState::Lifted,
            )
            .await
            .expect("mark lifted")
        {
            TransitionBuildPodResizeLifecycleResult::Transitioned(_) => {}
            other => panic!("unexpected lifted: {other:?}"),
        }
        cluster.reset_patch_counter();

        Self {
            leases: Arc::new(BuildLeaseRepository::new(db.clone())),
            db,
            permits,
            task_id: task.id,
            task_run_id,
            invocation_id,
            pod_uid,
            cluster,
        }
    }

    fn lease_identity(&self) -> LeaseIdentity {
        LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
            task_id: self.task_id.clone(),
            task_run_id: self.task_run_id.clone(),
            invocation_id: self.invocation_id.clone(),
        })
    }

    /// LEDGER 1: `build_leases`. Occupying means capacity is still bought.
    async fn build_lease_state(&self) -> Option<BuildLeaseState> {
        self.leases
            .get(&BuildLeaseKey {
                consumer_kind: djinn_db::BuildLeaseConsumerKind::TaskInvocation,
                consumer_id: self.invocation_id.clone(),
            })
            .await
            .expect("read build_leases")
            .map(|row| row.state)
    }

    /// LEDGER 2: `build_pod_permits`. Where the resize lifecycle lives.
    async fn permit_state(&self) -> BuildPodPermitState {
        self.permits
            .active(&self.task_run_id)
            .await
            .expect("read build_pod_permits")
            .map_or(BuildPodPermitState::Released, |row| row.state)
    }
}

fn status_millicores(cluster: &StoredTaskRunPod) -> Option<u64> {
    cluster.launcher_status_cpu().map(|raw| {
        djinn_k8s::pod_resize::CpuLimit::parse(&raw)
            .expect("a parseable quantity")
            .millis()
    })
}

/// A `DirectServices` over the real coordinator lease service, with the real
/// server-side drop bridge installed as the terminal gate.
fn direct_services(
    ledgers: &Ledgers,
    surface: Arc<dyn TaskRunPodSurface>,
    clock: Arc<dyn ResizeDropClock>,
    gate_budget: Duration,
) -> Arc<DirectServices> {
    let mut context = djinn_agent::test_helpers::agent_context_from_db(
        ledgers.db.clone(),
        CancellationToken::new(),
    );
    context.resize_drop = Some(Arc::new(
        TaskRunResizeDropBridge::with_surface(ledgers.db.clone(), surface, clock)
            .with_budget(gate_budget),
    ));
    let build_lease = Arc::new(BuildLeaseService::new(
        Arc::clone(&ledgers.leases),
        BUILD_SLOT_CAP,
    ));
    Arc::new(DirectServices::with_build_lease(
        context,
        CancellationToken::new(),
        build_lease,
    ))
}

/// Queue and grant one `task_invocation` lease so `build_leases` is occupying.
async fn occupying_lease(services: &DirectServices, identity: &LeaseIdentity) -> LeaseFencingToken {
    // Deadlines are ABSOLUTE epoch milliseconds, not durations — see
    // `djinn_agent::process::deadline_epoch_ms`. A relative value here reads as
    // "1970" and the queue answers `LeaseWaitTimeout`.
    // The same clock the coordinator compares deadlines against; `clippy.toml`
    // disallows reading the wall clock directly.
    let now_ms = djinn_coordinator::build_lease::SystemLeaseClock.now_ms();
    let queued = services
        .queue_lease(LeaseQueueRequest {
            identity: identity.clone(),
            deadlines: djinn_supervisor::services::LeaseDeadlines {
                queue_deadline_ms: now_ms + 600_000,
                launch_deadline_ms: now_ms + 3_600_000,
            },
        })
        .await;
    // With capacity free the queue grants immediately; under contention it
    // queues and the grant below picks the row up. Both shapes end occupying.
    let fence = match queued {
        LeaseResult::Granted(grant) => grant.fencing_token,
        LeaseResult::Queued(status) => {
            match services
                .grant_lease(LeaseGrantRequest {
                    identity: identity.clone(),
                    fencing_token: status.fencing_token.unwrap_or(LeaseFencingToken(0)),
                })
                .await
            {
                LeaseResult::Granted(grant) => grant.fencing_token,
                LeaseResult::Status(status) => status
                    .fencing_token
                    .expect("a granted lease carries a fencing token"),
                other => panic!("unexpected grant outcome: {other:?}"),
            }
        }
        other => panic!("unexpected queue outcome: {other:?}"),
    };
    assert!(
        !matches!(
            services
                .lease_status(djinn_supervisor::services::LeaseStatusRequest {
                    identity: identity.clone()
                })
                .await,
            LeaseResult::LeaseUnavailable
        ),
        "the lease must be readable before the terminal"
    );
    fence
}

// ── AC6: the lease is held across BOTH ledgers ─────────────────────────────

/// While the permit row is in `drop_required`/`drop_applying`/`quarantined`,
/// `BuildLeaseService::release` has NOT completed, the `build_leases` row is
/// still occupying, and terminal is not acknowledged.
///
/// The fault is a total apiserver outage, so the drop exhausts its confirmation
/// budget, quarantines, and can never confirm absence. That is the shape where
/// a released lease would be a Pod holding 4000m that nobody is paying for.
///
/// NAMED FAILING MUTATION: let `DirectServices::release_lease` call
/// `BuildLeaseService::release` unconditionally as it did before `0ppk-2`. The
/// `build_leases` assertions below then read `Terminal` and this test fails.
///
/// Note which ledger each assertion reads. Asserting only that the permit row
/// changed would be a success log from the wrong store.
#[tokio::test]
async fn a_quarantined_drop_holds_the_build_lease_in_the_other_ledger() {
    let ledgers = Ledgers::lifted("held").await;
    let surface = Arc::new(FixtureSurface(ledgers.cluster.clone()));
    let services = direct_services(
        &ledgers,
        surface,
        Arc::new(StallingClock::new(24)),
        Duration::from_millis(50),
    );

    let identity = ledgers.lease_identity();
    let fence = occupying_lease(&services, &identity).await;

    // LEDGER 1, before: capacity is bought.
    let before = ledgers
        .build_lease_state()
        .await
        .expect("the build lease row exists");
    assert!(
        !matches!(before, BuildLeaseState::Terminal),
        "the build lease must be occupying before the terminal: {before:?}"
    );

    // Take the apiserver away before starting the drop. The old spawned
    // `yield_now` raced the release future: under a loaded executor the gate
    // could spend its whole wait budget before the fault task ran, returning
    // while the durable permit was still `lifted`. Installing the GET fault
    // synchronously still exercises the same fail-closed path and guarantees
    // absence can never be confirmed once quarantine begins.
    ledgers.cluster.fail_gets(ApiFault::timeout());

    let released = services
        .release_lease(LeaseReleaseRequest {
            identity: identity.clone(),
            fencing_token: fence,
            candidate_cleanup: false,
        })
        .await;

    // The handler must NOT report a release.
    assert!(
        matches!(released, LeaseResult::LeaseUnavailable),
        "an unsettled drop must not acknowledge terminal: {released:?}"
    );

    // LEDGER 2: the permit is in a nonterminal resize state.
    let permit = ledgers.permit_state().await;
    assert!(
        matches!(
            permit,
            BuildPodPermitState::DropRequired
                | BuildPodPermitState::DropApplying
                | BuildPodPermitState::Quarantined
        ),
        "the permit must still owe a drop: {permit:?}"
    );

    // LEDGER 1, after — THE ASSERTION THAT MATTERS. `build_leases`, not
    // `build_pod_permits`. Capacity is still bought because the launcher is
    // still holding CPU nobody has proven it gave back.
    let after = ledgers
        .build_lease_state()
        .await
        .expect("the build lease row still exists");
    assert!(
        !matches!(after, BuildLeaseState::Terminal),
        "BuildLeaseService::release must NOT have completed while the permit is \
         in {permit:?}; build_leases reports {after:?}"
    );

    // And the CPU really is still lifted: the enforced fact, not a stored state.
    assert_eq!(
        status_millicores(&ledgers.cluster),
        Some(LIFTED_MILLICORES),
        "the launcher is still holding the lifted ceiling, which is exactly why \
         the lease may not be returned"
    );
}

/// The mirror image: a drop that IS confirmed lets the release through, and
/// both ledgers move — in that order.
///
/// Without this the test above would pass against a `release_lease` that always
/// refused, which would be a different bug rather than a fix.
#[tokio::test]
async fn a_confirmed_drop_releases_the_build_lease() {
    let ledgers = Ledgers::lifted("released").await;
    let surface = Arc::new(FixtureSurface(ledgers.cluster.clone()));
    let services = direct_services(
        &ledgers,
        surface,
        Arc::new(StallingClock::new(64)),
        Duration::from_secs(10),
    );

    let identity = ledgers.lease_identity();
    let fence = occupying_lease(&services, &identity).await;

    let released = services
        .release_lease(LeaseReleaseRequest {
            identity: identity.clone(),
            fencing_token: fence,
            candidate_cleanup: false,
        })
        .await;

    assert!(
        matches!(released, LeaseResult::Released { .. }),
        "a confirmed drop must release: {released:?}"
    );
    // LEDGER 2: back at the birth state.
    assert_eq!(
        ledgers.permit_state().await,
        BuildPodPermitState::BirthConfirmed
    );
    // LEDGER 1: capacity returned.
    assert_eq!(
        ledgers.build_lease_state().await,
        Some(BuildLeaseState::Terminal),
        "build_leases must move only after the drop is confirmed"
    );
    // THE ENFORCED FACT: the launcher's own init-container status.
    assert_eq!(
        status_millicores(&ledgers.cluster),
        Some(250),
        "the lease is only returned because the launcher actually came back down"
    );
}

// ── The reconciler must not race the worker it speaks for ──────────────────

/// A gate fixed at [`ResizeReconcileMode::Enforce`].
struct ArmedGate;

impl ResizeReconcileGate for ArmedGate {
    fn mode(&self) -> ResizeReconcileMode {
        ResizeReconcileMode::Enforce
    }
}

/// A surface that fires one full reconciler sweep from INSIDE the worker's own
/// drop, at the instant the durable row is at `drop_applying`.
///
/// This is the 2026-08-01 production interleaving, made deterministic. The
/// reconciler is a 30-second timer in production, so the race is decided by
/// where the tick lands; here it lands exactly where it landed in the incident —
/// after `apply_drop` wrote `drop_applying` and before the confirming read that
/// precedes `settle_confirmed`.
///
/// The reconciler drives the SAME stored Pod through its own bridge, so a
/// resumption really does PATCH and really does settle the ledger, exactly as
/// the second `task_run_resize_drop: returning the launcher to its birth limit`
/// line in the incident log did.
struct ReconcilerRacingSurface {
    cluster: StoredTaskRunPod,
    reconciler: Arc<TaskRunResizeReconciler>,
    observations: std::sync::atomic::AtomicU64,
    passes: std::sync::Mutex<Vec<ResizeReconcilePass>>,
}

#[async_trait]
impl TaskRunPodSurface for ReconcilerRacingSurface {
    async fn observe_launcher(
        &self,
        _task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        if self
            .observations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            let pass = self.reconciler.run_pass().await;
            self.passes.lock().expect("passes").push(pass);
        }
        self.cluster.observe_launcher()
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError> {
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

/// A reconciler tick landing inside the worker's own drop must not turn a drop
/// that SUCCEEDED into `LeaseUnavailable`.
///
/// The production failure, in one sentence: the reconciler resumed a
/// `drop_applying` row a live worker was already driving, won the
/// `drop_applying -> birth_confirmed` compare-and-swap, and the worker's own
/// settle was then rejected — so `release_lease` reported
/// `Unavailable("permit ... refused the DropApplying -> BirthConfirmed
/// transition")` and returned `LeaseUnavailable` for a launcher that was
/// demonstrably back at 250m. Three consecutive `LeaseUnavailable` responses
/// force `ResizeDropCause::LeaseUnavailableForcedCancel`, so a healthy task run
/// could be cancelled because the reconciler raced its own worker.
///
/// This is asserted at the COMPOSITION — `DirectServices::release_lease` — and
/// not against `strandedness`, because the harm is the RPC's answer. A test of
/// the classifier alone cannot see a lease that was refused.
///
/// NAMED FAILING MUTATION: restore
/// `DropRequired | DropApplying | Quarantined => Ok(Strandedness::DropOwed)` in
/// `task_run_resize_reconcile::strandedness`. The recorded pass then reports
/// `resumed = 1`, the permit ends at `birth_confirmed` before the worker's
/// settle runs, and `release_lease` answers `LeaseUnavailable` — this test's
/// first assertion — while `build_leases` stays occupying.
#[tokio::test]
async fn a_reconciler_tick_inside_the_workers_own_drop_does_not_refuse_the_lease() {
    let ledgers = Ledgers::lifted("racing-reconciler").await;
    // A LONG-RUNNING task run: the permit row is two hours old before the
    // worker's drop even begins. The grace must key on when the LIFECYCLE last
    // moved, not on when the permit was acquired — otherwise every real build's
    // row is permanently past any grace and this fix ships inert.
    ledgers
        .permits
        .backdate_state_change_for_test(&ledgers.task_run_id, Duration::from_secs(7200))
        .await
        .expect("age the permit row");

    // The reconciler's own view of the cluster: the same stored Pod, its own
    // bridge, exactly as the leader's sweep has in production.
    let reconciler = Arc::new(TaskRunResizeReconciler::new(
        ledgers.db.clone(),
        Arc::new(TaskRunResizeDropBridge::with_surface(
            ledgers.db.clone(),
            Arc::new(FixtureSurface(ledgers.cluster.clone())) as Arc<dyn TaskRunPodSurface>,
            Arc::new(StallingClock::new(64)) as Arc<dyn ResizeDropClock>,
        )),
        None,
        Arc::new(ArmedGate),
    ));
    let racing = Arc::new(ReconcilerRacingSurface {
        cluster: ledgers.cluster.clone(),
        reconciler: Arc::clone(&reconciler),
        observations: std::sync::atomic::AtomicU64::new(0),
        passes: std::sync::Mutex::new(Vec::new()),
    });
    let services = direct_services(
        &ledgers,
        Arc::clone(&racing) as Arc<dyn TaskRunPodSurface>,
        Arc::new(StallingClock::new(64)),
        Duration::from_secs(10),
    );

    let identity = ledgers.lease_identity();
    let fence = occupying_lease(&services, &identity).await;

    let released = services
        .release_lease(LeaseReleaseRequest {
            identity,
            fencing_token: fence,
            candidate_cleanup: false,
        })
        .await;

    // THE ASSERTION THE INCIDENT IS ABOUT.
    assert!(
        matches!(released, LeaseResult::Released { .. }),
        "a drop the worker completed must release, even though a reconciler \
         swept the row mid-drop: {released:?}"
    );

    // NON-VACUITY: the sweep really did run, really did see this row, and
    // really did decline it. Without this, a reconciler that was never invoked
    // would pass the assertion above for the wrong reason.
    let passes = racing.passes.lock().expect("passes").clone();
    assert_eq!(
        passes.len(),
        1,
        "exactly one sweep must have raced the drop"
    );
    let pass = &passes[0];
    assert_eq!(
        pass.scanned, 1,
        "the sweep must have SEEN the in-flight row; pass was {pass:?}"
    );
    assert_eq!(
        pass.resumed, 0,
        "the sweep must not resume a drop its own worker is performing; pass \
         was {pass:?}"
    );
    assert_eq!(
        pass.skipped,
        vec![(ledgers.task_run_id.clone(), SkipReason::OwnerLive)],
        "pass was {pass:?}"
    );

    // Both ledgers, and the enforced fact.
    assert_eq!(
        ledgers.permit_state().await,
        BuildPodPermitState::BirthConfirmed
    );
    assert_eq!(
        ledgers.build_lease_state().await,
        Some(BuildLeaseState::Terminal),
        "capacity must be returned for a drop that succeeded"
    );
    assert_eq!(
        status_millicores(&ledgers.cluster),
        Some(250),
        "the launcher really did come back down, which is why refusing the \
         lease was always wrong"
    );
}

// ── AC2: concurrency is decided, not assumed ───────────────────────────────

struct LiftSurface(StoredTaskRunPod);

#[async_trait]
impl djinn_coordinator::resize_lift::LauncherResizeSurface for LiftSurface {
    async fn observe_launcher(
        &self,
        _task_run_id: &str,
    ) -> Result<Option<ObservedLauncherSidecar>, LauncherObservationError> {
        self.0.observe_launcher()
    }

    async fn resize_launcher_cpu(
        &self,
        pod_name: &str,
        target_millicores: u64,
    ) -> Result<(), PodResizeError> {
        self.0
            .resize_launcher_cpu(pod_name, target_millicores)
            .await
    }
}

/// **THE ANSWER: two invocations of one task run CAN be in flight at once.**
///
/// `djinn_agent::process`'s `'invocation: loop` is a sequential poll loop over
/// ONE child, but `LeaseInvocationRunner::output` takes `&self` on an
/// `Arc`-shared runner with no lock, no permit and no "current invocation" slot;
/// the invocation journal keeps a `Mutex<HashSet<String>>` of LIVE invocation
/// ids rather than an `Option`; and two production entry points reach it without
/// mutual exclusion (the shell tool handler and the worker's brokered
/// cargo-target seed). Nothing serializes invocations of one task run.
///
/// `build_pod_permits` has PRIMARY KEY `task_run_id`, so a single row cannot
/// represent two simultaneous lifts. The second invocation must therefore FAIL
/// CLOSED — with a named degraded reason, and **without issuing a PATCH**,
/// because a PATCH from a lift that is about to be refused would raise a ceiling
/// nobody is going to drop.
///
/// NAMED FAILING MUTATION: delete the `Lifted -> Lifted` invocation-fenced
/// self-transition from `ResizeLift::apply`'s `Lifted` arm — the guard this
/// answer rests on. The second invocation then re-confirms a lift it does not
/// own and `resize_patches()` becomes non-zero.
#[tokio::test]
async fn a_second_concurrent_invocation_fails_closed_and_issues_zero_patches() {
    use djinn_coordinator::resize_authorization::{PodResizeApplier, PodResizeIntent};
    use djinn_coordinator::resize_lift::ResizeLift;
    use djinn_launcher_protocol::LauncherAuthorityProtocol;
    use djinn_supervisor::services::DegradedUnleasedReason;

    let ledgers = Ledgers::lifted("concurrent").await;
    let row = ledgers
        .permits
        .active(&ledgers.task_run_id)
        .await
        .expect("read permit")
        .expect("permit row");
    assert_eq!(row.state, BuildPodPermitState::Lifted);
    assert_eq!(
        row.resize_invocation_id.as_deref(),
        Some(ledgers.invocation_id.as_str())
    );

    let surface = Arc::new(LiftSurface(ledgers.cluster.clone()));
    let lift = ResizeLift::with_surface(Arc::clone(&ledgers.permits), surface);

    // Invocation B authorized while A already held `lifted` — the exact race
    // the single-row lifecycle cannot represent.
    let intruder = PodResizeIntent {
        task_run_id: ledgers.task_run_id.clone(),
        invocation_id: uuid::Uuid::now_v7().to_string(),
        permit_id: row.permit_id.clone(),
        fencing_token: row.fencing_token,
        permit_state: BuildPodPermitState::Lifted,
        pod_namespace: "djinn".to_owned(),
        pod_name: format!("taskrun-{}", ledgers.pod_uid),
        pod_uid: ledgers.pod_uid.clone(),
        launcher_container_name: "cgroup-launcher".to_owned(),
        launcher_container_id: "containerd://launcher".to_owned(),
        effective_launcher_protocol: LauncherAuthorityProtocol::ResizeV2,
        target_millicores: CEILING_MILLICORES,
        admitted_cpu_millicores: CEILING_MILLICORES,
    };

    let failure = lift
        .apply(&intruder)
        .await
        .expect_err("the second invocation must fail closed");
    assert_eq!(
        failure.reason,
        DegradedUnleasedReason::LiftLifecycleUnwritable,
        "the refusal must be a NAMED degraded reason, not a bare error"
    );
    assert_eq!(
        ledgers.cluster.resize_patches(),
        0,
        "a lift that does not own the lifecycle must issue ZERO PATCHes"
    );
    // And the row still belongs to the invocation that actually claimed it.
    let after = ledgers
        .permits
        .active(&ledgers.task_run_id)
        .await
        .expect("read permit")
        .expect("permit row");
    assert_eq!(after.state, BuildPodPermitState::Lifted);
    assert_eq!(
        after.resize_invocation_id.as_deref(),
        Some(ledgers.invocation_id.as_str()),
        "the loser must never re-key the row"
    );
}

/// A run whose Pod UID is confirmed absent releases too — the second of the two
/// facts the epic permits.
#[tokio::test]
async fn an_absent_pod_uid_releases_the_build_lease() {
    let ledgers = Ledgers::lifted("absent").await;
    ledgers
        .cluster
        .uid_fenced_delete(&ledgers.task_run_id, &ledgers.pod_uid)
        .expect("the Pod dies before the terminal");
    let surface = Arc::new(FixtureSurface(ledgers.cluster.clone()));
    let services = direct_services(
        &ledgers,
        surface,
        Arc::new(StallingClock::new(64)),
        Duration::from_secs(10),
    );

    let identity = ledgers.lease_identity();
    let fence = occupying_lease(&services, &identity).await;
    let released = services
        .release_lease(LeaseReleaseRequest {
            identity,
            fencing_token: fence,
            candidate_cleanup: false,
        })
        .await;

    assert!(
        matches!(released, LeaseResult::Released { .. }),
        "a Pod UID confirmed absent releases: {released:?}"
    );
    assert_eq!(
        ledgers.build_lease_state().await,
        Some(BuildLeaseState::Terminal)
    );
    assert_eq!(
        ledgers.cluster.resize_patches(),
        0,
        "no PATCH may be addressed to a Pod that does not exist"
    );
}
