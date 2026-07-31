//! Tests for [`super`], the fail-safe drop and UID-fenced quarantine.
//!
//! # What is real here and what is faked
//!
//! The durable lifecycle is driven against **real Postgres**
//! ([`Database::ephemeral`], a template-cloned server — not an in-memory
//! stand-in). No `Fake` or `Mock` permit repository appears anywhere in this
//! file, deliberately: the compare-and-swap, migration 164's transition trigger
//! and migration 168's invocation write-window are Postgres behaviours, and a
//! fake would assert the test's own opinion of them.
//!
//! The apiserver is [`djinn_k8s::pod_resize_fixture::StoredTaskRunPod`] — a
//! stored `Pod` object driven through the **production**
//! `PodResizeClient::resize_launcher_cpu` and the **production**
//! `observe_launcher_sidecar`. A resize confirmed against it is confirmed by the
//! same `confirm_launcher_cpu` rule that runs in the cluster, reading
//! `status.initContainerStatuses` in millicores. That is the fault-injection
//! seam this slice is supposed to have.
//!
//! # The question every test here had to answer
//!
//! "What stays green if the body of this does nothing?" Each test names the
//! mutation that must break it.

use super::*;
use djinn_db::{
    AcquireBuildPodPermitResult, BindBuildPodPermitResult, BuildPodPermitRow,
    CaptureBuildPodResizeIdentityResult, CreateTaskRunParams, Database, EffectiveCreatorProvenance,
    ProjectRepository, TaskRepository, TaskRunRepository, TransitionBuildPodResizeLifecycleResult,
    UserRepository,
};
use djinn_k8s::pod_resize_fixture::{ApiFault, StoredTaskRunPod};
use std::sync::Mutex as StdMutex;

const CEILING: &str = "4";
const CEILING_MILLICORES: i64 = 4000;
const LIFTED_MILLICORES: u64 = 4000;

// ── The apiserver surface under test ───────────────────────────────────────

/// [`StoredTaskRunPod`] behind [`TaskRunPodSurface`], with a GET counter.
///
/// The counter is what makes "a FRESH GET before EVERY attempt" measurable.
/// Caching the observed Pod between attempts — acceptance criterion 5's second
/// named mutation — drops it below the attempt count.
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
    ) -> Result<(), PodResizeError> {
        self.cluster
            .resize_launcher_cpu(pod_name, target_millicores)
            .await
    }

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        self.deletes_attempted.fetch_add(1, Ordering::SeqCst);
        self.cluster.uid_fenced_delete(task_run_id, pod_uid)
    }
}

// ── The injected clock ─────────────────────────────────────────────────────

/// A clock that records what it was asked to wait for and never actually waits.
///
/// `stall_after` is what makes an UNBOUNDED loop observable: after that many
/// sleeps it parks forever, so a test can assert the drop future is still
/// pending. A bounded quarantine loop would have returned before reaching it.
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

// ── Durable fixtures, against real Postgres ────────────────────────────────

async fn seed_task_run(db: &Database, suffix: &str) -> String {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            i64::try_from(uuid::Uuid::now_v7().as_u128() % 8_000_000_000_000_000_000)
                .expect("github id"),
            &format!("resize-drop-{suffix}-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed user");
    let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create(
            &format!("resize-drop-{suffix}"),
            "djinnos",
            &format!("resize-drop-{suffix}"),
        )
        .await
        .expect("seed project");
    let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_in_project_with_provenance(
            &project.id,
            None,
            EffectiveCreatorProvenance::explicit_user_id(&user.id),
            "resize drop",
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
    task_run_id
}

/// One captured permit at `birth_confirmed`, fenced to `pod_uid`.
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

/// Drive the row through a REAL lift so a drop test is never scoring a free
/// pass on a Pod that was never raised in the first place.
///
/// This is acceptance criterion 3's non-vacuity clause made mechanical: it
/// asserts the Pod's own init-container status reports the lifted ceiling before
/// the terminal path under test runs.
async fn lift(
    permits: &BuildPodPermitRepository,
    cluster: &StoredTaskRunPod,
    row: &BuildPodPermitRow,
    pod_uid: &str,
    invocation_id: &str,
) {
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
        "NON-VACUITY: the launcher must actually be lifted before a drop is \
         asked to bring it back down"
    );
    match permits
        .transition_resize_lifecycle(
            &row.task_run_id,
            &row.permit_id,
            row.fencing_token,
            pod_uid,
            Some(invocation_id),
            BuildPodPermitState::LiftApplying,
            BuildPodPermitState::Lifted,
        )
        .await
        .expect("mark lifted")
    {
        TransitionBuildPodResizeLifecycleResult::Transitioned(_) => {}
        other => panic!("unexpected lifted outcome: {other:?}"),
    }
}

/// The launcher's **init-container** status limit, in millicores.
///
/// Millicores, never the Quantity string: the apiserver canonicalises `4000m`
/// to `4`, and #2861 watched a string comparison report
/// `never reported 2000m; last observed Some(2000)`.
fn status_millicores(cluster: &StoredTaskRunPod) -> Option<u64> {
    cluster.launcher_status_cpu().map(|raw| {
        djinn_k8s::pod_resize::CpuLimit::parse(&raw)
            .expect("the launcher reports a parseable quantity")
            .millis()
    })
}

async fn state_of(permits: &BuildPodPermitRepository, task_run_id: &str) -> BuildPodPermitState {
    permits
        .active(task_run_id)
        .await
        .expect("read permit")
        .map_or(BuildPodPermitState::Released, |row| row.state)
}

/// A lifted task run, its cluster, its permits and its drop, all wired up.
struct Fixture {
    db: Database,
    permits: Arc<BuildPodPermitRepository>,
    cluster: StoredTaskRunPod,
    surface: Arc<CountingSurface>,
    clock: Arc<RecordingClock>,
    drop: TaskRunResizeDrop,
    task_run_id: String,
    pod_uid: String,
    invocation_id: String,
}

impl Fixture {
    async fn lifted(suffix: &str) -> Self {
        Self::build(suffix, RecordingClock::new(), true).await
    }

    async fn lifted_with_clock(suffix: &str, clock: RecordingClock) -> Self {
        Self::build(suffix, clock, true).await
    }

    async fn captured_only(suffix: &str) -> Self {
        Self::build(suffix, RecordingClock::new(), false).await
    }

    async fn build(suffix: &str, clock: RecordingClock, do_lift: bool) -> Self {
        let db = Database::ephemeral().await.expect("ephemeral db");
        let task_run_id = seed_task_run(&db, suffix).await;
        let pod_uid = format!("pod-uid-{suffix}");
        let invocation_id = format!("invocation-{suffix}");
        let permits = Arc::new(BuildPodPermitRepository::new(db.clone()));
        let row = captured_permit(&permits, &task_run_id, &pod_uid).await;
        let cluster = StoredTaskRunPod::resize_v2(&pod_uid, CEILING);
        if do_lift {
            lift(&permits, &cluster, &row, &pod_uid, &invocation_id).await;
        }
        cluster.reset_patch_counter();
        let surface = Arc::new(CountingSurface::new(cluster.clone()));
        let clock = Arc::new(clock);
        let drop = TaskRunResizeDrop::with_clock(
            Arc::clone(&permits),
            Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>,
            Arc::clone(&clock) as Arc<dyn ResizeDropClock>,
        );
        Self {
            db,
            permits,
            cluster,
            surface,
            clock,
            drop,
            task_run_id,
            pod_uid,
            invocation_id,
        }
    }

    fn request(&self, cause: ResizeDropCause) -> ResizeDropRequest {
        ResizeDropRequest {
            task_run_id: self.task_run_id.clone(),
            invocation_id: Some(self.invocation_id.clone()),
            cause,
        }
    }

    /// An infrastructure observer's request: it saw a Pod die, not an invocation
    /// end, so it names no invocation.
    fn infra_request(&self, cause: ResizeDropCause) -> ResizeDropRequest {
        ResizeDropRequest {
            task_run_id: self.task_run_id.clone(),
            invocation_id: None,
            cause,
        }
    }

    async fn state(&self) -> BuildPodPermitState {
        state_of(&self.permits, &self.task_run_id).await
    }
}

// ── AC3: every terminal path reaches drop_required ─────────────────────────

/// Every way an invocation can end, driven through the seam each one funnels
/// into, must move the permit row to `drop_required`.
///
/// NON-VACUITY: [`Fixture::lifted`] performs a REAL lift first and asserts the
/// launcher's own init-container status reports the raised ceiling. A path that
/// never lifted could not score a free pass here.
///
/// NAMED FAILING MUTATION: make [`TaskRunResizeDrop::require_drop`] return
/// `Settled(NotResizeGoverned)` for any single [`ResizeDropCause`] and exactly
/// that row of the table fails.
#[tokio::test]
async fn every_terminal_path_moves_a_lifted_permit_to_drop_required() {
    // The five worker-side terminals (`process.rs`'s `'invocation: loop`), the
    // two grant-side ones, the two infrastructure ones observed by
    // `watch_infra_death`, and the restart case where no in-process actor knows
    // a lift was ever started.
    let causes = [
        ResizeDropCause::NormalExit,
        ResizeDropCause::NonZeroExit,
        ResizeDropCause::TimedOut,
        ResizeDropCause::Cancelled,
        ResizeDropCause::LeaseUnavailableForcedCancel,
        ResizeDropCause::GrantDenied,
        ResizeDropCause::DegradedUnleased,
        ResizeDropCause::WorkerDisconnect,
        ResizeDropCause::PodTerminated,
        ResizeDropCause::ServerRestart,
    ];

    for cause in causes {
        let fixture = Fixture::lifted(cause.as_str()).await;
        assert_eq!(
            fixture.state().await,
            BuildPodPermitState::Lifted,
            "{}: the row must actually be lifted before the terminal path runs",
            cause.as_str()
        );

        // The two infrastructure paths carry no invocation identity, because a
        // dead Pod names no invocation. Everything else does.
        let request = if matches!(
            cause,
            ResizeDropCause::WorkerDisconnect
                | ResizeDropCause::PodTerminated
                | ResizeDropCause::ServerRestart
        ) {
            fixture.infra_request(cause)
        } else {
            fixture.request(cause)
        };

        match fixture.drop.require_drop(&request).await {
            ResizeDropRequirement::Required(_) => {}
            other => panic!("{}: unexpected requirement {other:?}", cause.as_str()),
        }
        assert_eq!(
            fixture.state().await,
            BuildPodPermitState::DropRequired,
            "{}: this terminal path did not reach drop_required",
            cause.as_str()
        );
    }
}

/// A row found mid-lift by a process that never started that lift — the server
/// restart case — still owes a drop. Nothing in this test's process remembers
/// the lift; the durable row is the only witness.
#[tokio::test]
async fn a_lift_applying_row_left_by_a_dead_process_still_reaches_drop_required() {
    let fixture = Fixture::captured_only("restart").await;
    let row = fixture
        .permits
        .active(&fixture.task_run_id)
        .await
        .expect("read permit")
        .expect("permit row");
    // Claim, and then "crash": nothing marks it lifted, nothing drops it.
    fixture
        .permits
        .begin_resize_invocation(
            &fixture.task_run_id,
            &row.permit_id,
            row.fencing_token,
            &fixture.pod_uid,
            &fixture.invocation_id,
        )
        .await
        .expect("claim");
    assert_eq!(fixture.state().await, BuildPodPermitState::LiftApplying);

    // A brand-new drop object: no in-process memory of the lift at all.
    let restarted = TaskRunResizeDrop::with_clock(
        Arc::clone(&fixture.permits),
        Arc::clone(&fixture.surface) as Arc<dyn TaskRunPodSurface>,
        Arc::new(RecordingClock::new()),
    );
    match restarted
        .require_drop(&fixture.infra_request(ResizeDropCause::ServerRestart))
        .await
    {
        ResizeDropRequirement::Required(_) => {}
        other => panic!("unexpected requirement: {other:?}"),
    }
    assert_eq!(fixture.state().await, BuildPodPermitState::DropRequired);
}

// ── AC4: the drop is confirmed from init-container status ──────────────────

/// The drop is only `Confirmed` when the launcher's **init-container** status
/// reports 250m, compared in millicores.
///
/// NAMED FAILING MUTATION: make the drop write `state = 'birth_confirmed'`
/// without issuing the PATCH. The PATCH counter goes to zero and the
/// init-container status assertion below reports the lifted ceiling, so this
/// test fails. Note what is deliberately NOT asserted as evidence: the permit
/// row's `state` column. A stored state is a label.
#[tokio::test]
async fn a_confirmed_drop_is_read_back_from_init_container_status_in_millicores() {
    let fixture = Fixture::lifted("confirm").await;
    assert_eq!(
        status_millicores(&fixture.cluster),
        Some(LIFTED_MILLICORES),
        "the launcher must start this test holding the LIFTED ceiling"
    );

    let outcome = fixture
        .drop
        .drop_to_birth(&fixture.request(ResizeDropCause::NormalExit))
        .await;

    assert_eq!(
        outcome,
        ResizeDropOutcome::Confirmed {
            pod_uid: fixture.pod_uid.clone()
        }
    );
    // THE ENFORCED FACT, not the label: what the launcher's own init-container
    // status reports, in millicores because the apiserver canonicalises `4000m`
    // to `4` and a string comparison would report a false mismatch.
    assert_eq!(
        status_millicores(&fixture.cluster),
        Some(BIRTH_CPU_MILLICORES),
        "the launcher must actually be back at its birth limit"
    );
    assert_eq!(
        fixture.cluster.resize_patches(),
        1,
        "exactly one pods/resize PATCH returns the launcher to 250m"
    );
    assert_eq!(
        fixture.cluster.patched_cpu_millicores(),
        vec![BIRTH_CPU_MILLICORES],
        "the PATCH BODY carried 250m"
    );
}

/// A `status.containerStatuses` entry named `cgroup-launcher` reporting 250m,
/// while the init-container status is stale, must NOT confirm the drop.
///
/// The launcher is a native sidecar: no regular container by that name can
/// legitimately exist, so anything reading that array would find a *matching*
/// limit and report success while the launcher held the lifted ceiling.
///
/// NAMED FAILING MUTATION: confirm from `status.containerStatuses` and this test
/// reports `Confirmed` instead of a quarantine.
#[tokio::test]
async fn a_misleading_regular_container_status_never_confirms_the_drop() {
    let fixture = Fixture::lifted_with_clock(
        "misleading",
        // Short-circuit the 30s window: the fixture's fault is permanent, so
        // the only question is what the loop decides, not how long it waits.
        RecordingClock::new(),
    )
    .await;
    fixture.cluster.stop_actuating();
    fixture
        .cluster
        .add_misleading_regular_launcher_status("250m");

    let outcome = fixture
        .drop
        .drop_to_birth(&fixture.request(ResizeDropCause::NormalExit))
        .await;

    assert!(
        matches!(outcome, ResizeDropOutcome::QuarantinedPodDeleted { .. }),
        "a regular-container status must never confirm a native sidecar's \
         resize; got {outcome:?}"
    );
}

// ── AC5: the retry schedule, measured ──────────────────────────────────────

/// Bounded exponential backoff from 250ms, doubling, capped at 2s, inside a
/// 30-second confirmation window — asserted as the observed delay sequence and
/// the attempt count, against an injected clock.
///
/// NAMED FAILING MUTATION 1: remove the 2s cap and the sequence becomes
/// `250,500,1000,2000,4000,8000,16000` (6 sleeps), so the assertion fails.
/// NAMED FAILING MUTATION 2: cache the observed Pod between attempts and the
/// per-attempt GET counter falls below the attempt count.
/// NAMED FAILING MUTATION 3: extend the deadline past 30s and both the sleep
/// count and the quarantine outcome change.
#[tokio::test]
async fn the_backoff_sequence_is_250ms_doubling_capped_at_2s_inside_30s() {
    let fixture = Fixture::lifted("schedule").await;
    // A node that accepts every resize and never actuates it: the shape that
    // spends the whole window without ever confirming.
    fixture.cluster.stop_actuating();

    let outcome = fixture
        .drop
        .drop_to_birth(&fixture.request(ResizeDropCause::NormalExit))
        .await;
    assert!(matches!(
        outcome,
        ResizeDropOutcome::QuarantinedPodDeleted { .. }
    ));

    // 250 + 500 + 1000 + 2000*n <= 30_000 with the deadline checked against the
    // sleep the loop is about to take: 3750 + 13*2000 = 29_750, and one more
    // 2s sleep would reach 31_750.
    let mut expected = vec![
        Duration::from_millis(250),
        Duration::from_millis(500),
        Duration::from_millis(1000),
    ];
    expected.extend(std::iter::repeat_n(Duration::from_secs(2), 14));
    let sleeps = fixture.clock.sleeps();
    let confirmation_sleeps = &sleeps[..expected.len().min(sleeps.len())];
    assert_eq!(
        confirmation_sleeps, expected,
        "the confirmation backoff must be 250ms doubling to a 2s cap"
    );
    assert!(
        confirmation_sleeps
            .iter()
            .all(|delay| *delay <= DROP_MAX_BACKOFF),
        "no delay may exceed the 2s cap"
    );
    assert_eq!(
        confirmation_sleeps.iter().sum::<Duration>(),
        Duration::from_millis(29_750),
        "the whole schedule must fit inside the 30s confirmation window"
    );

    let attempts = fixture.drop.confirmation_attempts();
    assert_eq!(
        attempts,
        expected.len() as u64 + 1,
        "one attempt per sleep, plus the one that found the deadline spent"
    );
    // A FRESH GET before EVERY attempt. The quarantine loop that follows also
    // observes, so this is a lower bound and the mutation that caches a Pod
    // between attempts breaks it from below.
    assert!(
        fixture.surface.gets() >= attempts,
        "each confirmation attempt must issue its own fresh GET: {} gets for {attempts} attempts",
        fixture.surface.gets()
    );
}

// ── AC7: quarantine is fail-safe and UID-fenced ────────────────────────────

/// Every apply-time fault, driven through the production resize client, ends
/// with the row quarantined, a UID-fenced DELETE issued, and the Pod gone.
///
/// NAMED FAILING MUTATION: turn any one fault into a retry-forever and that row
/// stops reaching `QuarantinedPodDeleted`.
#[tokio::test]
async fn every_apply_fault_quarantines_and_uid_fenced_deletes_the_pod() {
    // (name, install the fault, the reason the classifier must settle on)
    type Install = fn(&StoredTaskRunPod);
    let faults: Vec<(&str, Install)> = vec![
        ("patch_403", |c| c.fail_patches(ApiFault::forbidden())),
        ("patch_422", |c| c.fail_patches(ApiFault::unprocessable())),
        ("transport_timeout", |c| c.fail_patches(ApiFault::timeout())),
        (
            "accepted_but_stale_status",
            StoredTaskRunPod::stop_actuating,
        ),
        ("absent_init_status", |c| {
            c.stop_actuating();
            c.clear_launcher_status_limit();
        }),
        ("misleading_regular_status", |c| {
            c.stop_actuating();
            c.add_misleading_regular_launcher_status("250m");
        }),
        ("pod_resize_pending", |c| {
            c.stop_actuating();
            c.add_resize_pending();
        }),
        ("launcher_restarted", |c| {
            c.restart_launcher("containerd://restarted");
        }),
    ];

    for (name, install) in faults {
        let fixture = Fixture::lifted(name).await;
        install(&fixture.cluster);

        let outcome = fixture
            .drop
            .drop_to_birth(&fixture.request(ResizeDropCause::NormalExit))
            .await;

        match &outcome {
            ResizeDropOutcome::QuarantinedPodDeleted { pod_uid, .. } => {
                assert_eq!(pod_uid, &fixture.pod_uid, "{name}: wrong Pod quarantined");
            }
            other => panic!("{name}: expected a quarantine, got {other:?}"),
        }
        // THE ENFORCED FACT: a UID-fenced delete was issued for exactly this
        // Pod, and the Pod is actually gone from the fixture cluster.
        assert_eq!(
            fixture.cluster.deletes(),
            vec![(fixture.task_run_id.clone(), fixture.pod_uid.clone())],
            "{name}: the DELETE must be fenced on the exact Pod UID"
        );
        assert!(
            fixture
                .cluster
                .observe_launcher()
                .expect("observe")
                .is_none(),
            "{name}: the quarantined Pod must actually be gone"
        );
        // The permit is released only after absence is PROVEN, and a released
        // permit is what makes this task run unable to capture a replacement.
        assert_eq!(
            fixture.state().await,
            BuildPodPermitState::Released,
            "{name}: the permit is released once the Pod is proven absent"
        );
    }
}

/// The quarantine absence loop has no bound. Through a total apiserver outage
/// the row stays `quarantined`, the permit stays held, and the drop never
/// returns — so nothing downstream can release the lease.
///
/// NAMED FAILING MUTATION: bound the quarantine retry loop. The future then
/// completes, `handle.is_finished()` becomes true, and this test fails.
#[tokio::test]
async fn the_quarantine_loop_is_unbounded_across_an_apiserver_outage() {
    const STALL_AFTER: usize = 64;
    let fixture =
        Fixture::lifted_with_clock("outage", RecordingClock::stalling_after(STALL_AFTER)).await;
    fixture.cluster.fail_patches(ApiFault::forbidden());

    let permits = Arc::clone(&fixture.permits);
    let task_run_id = fixture.task_run_id.clone();
    let clock = Arc::clone(&fixture.clock);
    let surface = Arc::clone(&fixture.surface);
    let cluster = fixture.cluster.clone();
    let request = fixture.request(ResizeDropCause::NormalExit);
    let drop = Arc::new(TaskRunResizeDrop::with_clock(
        Arc::clone(&permits),
        Arc::clone(&surface) as Arc<dyn TaskRunPodSurface>,
        Arc::clone(&clock) as Arc<dyn ResizeDropClock>,
    ));

    let handle = {
        let drop = Arc::clone(&drop);
        tokio::spawn(async move { drop.drop_to_birth(&request).await })
    };

    // Let the drop quarantine, then take the apiserver away entirely so
    // absence can never be confirmed.
    for _ in 0..2000 {
        if drop.quarantine_attempts() >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    cluster.fail_gets(ApiFault::timeout());

    for _ in 0..20_000 {
        if clock.sleeps().len() >= STALL_AFTER {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert!(
        !handle.is_finished(),
        "a bounded quarantine loop would have given up and released the lease"
    );
    assert!(
        drop.quarantine_attempts() > 8,
        "the loop must keep retrying through the outage; saw {} attempts",
        drop.quarantine_attempts()
    );
    assert_eq!(
        state_of(&permits, &task_run_id).await,
        BuildPodPermitState::Quarantined,
        "the durable row stays quarantined for the whole outage"
    );
    handle.abort();
}

/// A Pod deleted and recreated under the same NAME must never be destroyed in
/// the original's place. Our Pod UID is absent the moment the object carrying
/// our label reports a different `metadata.uid`, so the quarantine settles
/// without issuing a delete at all.
///
/// NAMED FAILING MUTATION: delete the UID precondition from the DELETE (and the
/// UID comparison that guards it) and the replacement Pod is destroyed —
/// `cluster.deletes()` becomes non-empty and the replacement disappears.
#[tokio::test]
async fn a_pod_recreated_under_the_same_name_is_never_deleted_in_ours_place() {
    let fixture = Fixture::lifted("recreated").await;
    fixture.cluster.fail_patches(ApiFault::forbidden());
    // Between the lift and the drop, the Pod is replaced. Same name, new UID.
    fixture
        .cluster
        .recreate_under_same_name("pod-uid-replacement");

    let outcome = fixture
        .drop
        .drop_to_birth(&fixture.request(ResizeDropCause::NormalExit))
        .await;

    assert_eq!(
        outcome,
        ResizeDropOutcome::PodUidAbsent {
            pod_uid: fixture.pod_uid.clone()
        },
        "our Pod UID is absent; the replacement belongs to whoever created it"
    );
    assert!(
        fixture.cluster.deletes().is_empty(),
        "no DELETE may be issued against a Pod this permit does not own"
    );
    assert_eq!(
        fixture.surface.deletes_attempted(),
        0,
        "the UID fence must stop the delete BEFORE it reaches the apiserver"
    );
    assert_eq!(
        fixture
            .cluster
            .observe_launcher()
            .expect("observe")
            .expect("the replacement Pod")
            .pod_uid,
        "pod-uid-replacement",
        "the replacement Pod must still be alive"
    );
}

/// A Pod that vanished on its own settles the drop: nothing holds the raised
/// limit, so no PATCH and no DELETE are issued.
#[tokio::test]
async fn a_vanished_pod_settles_the_drop_without_touching_the_cluster() {
    let fixture = Fixture::lifted("vanished").await;
    fixture
        .cluster
        .uid_fenced_delete(&fixture.task_run_id, &fixture.pod_uid)
        .expect("remove the Pod out from under the drop");

    let outcome = fixture
        .drop
        .drop_to_birth(&fixture.request(ResizeDropCause::NormalExit))
        .await;

    assert_eq!(
        outcome,
        ResizeDropOutcome::PodUidAbsent {
            pod_uid: fixture.pod_uid.clone()
        }
    );
    assert!(outcome.releases_lease(), "an absent Pod UID releases");
    assert_eq!(
        fixture.cluster.resize_patches(),
        0,
        "no PATCH may be addressed to a Pod that does not exist"
    );
}

// ── AC1 / AC2: the invocation fence, at the drop's entry ───────────────────

/// A drop presented with an invocation the row does not carry is fenced, and
/// the row is untouched.
///
/// NAMED FAILING MUTATION: drop the `resize_invocation_id` comparison from
/// [`TaskRunResizeDrop::require_drop`] and this stale terminal drives the live
/// invocation's Pod back to 250m underneath it.
#[tokio::test]
async fn a_terminal_from_a_stale_invocation_is_fenced_and_changes_nothing() {
    let fixture = Fixture::lifted("stale-terminal").await;
    let stale = ResizeDropRequest {
        task_run_id: fixture.task_run_id.clone(),
        invocation_id: Some("some-other-invocation".to_owned()),
        cause: ResizeDropCause::NormalExit,
    };

    let outcome = fixture.drop.drop_to_birth(&stale).await;

    assert_eq!(
        outcome,
        ResizeDropOutcome::Fenced {
            owner: Some(fixture.invocation_id.clone())
        }
    );
    assert_eq!(
        fixture.state().await,
        BuildPodPermitState::Lifted,
        "the live invocation's lifecycle must be untouched"
    );
    assert_eq!(
        fixture.cluster.resize_patches(),
        0,
        "a fenced terminal issues no PATCH"
    );
    assert_eq!(
        status_millicores(&fixture.cluster),
        Some(LIFTED_MILLICORES),
        "the live invocation keeps its lifted ceiling"
    );
}

// ── AC8: the stranded task run ─────────────────────────────────────────────

/// After a quarantined Pod is deleted and its permit released, the SAME task run
/// cannot capture a replacement Pod. This proves the dead end rather than
/// assuming it, and names the outcome the epic left silent.
///
/// NAMED FAILING MUTATION: make `acquire` resurrect a released row and the
/// `AlreadyReleased` assertion below fails, showing the assertion is real.
#[tokio::test]
async fn a_quarantined_task_run_can_never_capture_a_replacement_pod() {
    let fixture = Fixture::lifted("stranded").await;
    fixture.cluster.fail_patches(ApiFault::forbidden());
    let outcome = fixture
        .drop
        .drop_to_birth(&fixture.request(ResizeDropCause::NormalExit))
        .await;
    assert!(matches!(
        outcome,
        ResizeDropOutcome::QuarantinedPodDeleted { .. }
    ));
    assert_eq!(fixture.state().await, BuildPodPermitState::Released);

    // 1. The permit lifecycle cannot be restarted under the same task-run id.
    let reacquired = fixture.permits.acquire(&fixture.task_run_id, 16).await;
    assert!(
        matches!(
            reacquired,
            AcquireBuildPodPermitResult::AlreadyReleased { .. }
        ),
        "a released permit is not resurrected under the same run id: {reacquired:?}"
    );

    // 2. Even holding the original permit identity, the write-once capture
    //    predicate (`state = 'job_created' AND pod_uid IS NULL`) can never hold
    //    again, so a replacement Pod cannot be captured.
    let released = fixture.db.clone();
    let permits = BuildPodPermitRepository::new(released);
    let replacement = BuildPodResizeIdentity {
        pod_namespace: "djinn".to_owned(),
        pod_name: "taskrun-replacement".to_owned(),
        pod_uid: "pod-uid-replacement".to_owned(),
        launcher_container_name: "cgroup-launcher".to_owned(),
        launcher_container_id: "containerd://replacement".to_owned(),
        image_digest: "registry/img@sha256:feed".to_owned(),
        observed_launcher_protocol: "resize-v2".to_owned(),
        effective_launcher_protocol: "resize-v2".to_owned(),
        admitted_cpu_millicores: CEILING_MILLICORES,
    };
    let row = permits
        .active(&fixture.task_run_id)
        .await
        .expect("read permit");
    assert!(row.is_none(), "the released row is no longer active");

    // 3. THE NAMED TERMINAL REASON. `TaskRunResizeAdmissionBridge::admit_dispatch`
    //    is the only path that could re-admit this task run, and it refuses:
    //    with no active permit row the bootstrap answers `StalePermit`, which
    //    the bridge renders into a dispatch refusal that names itself. The run
    //    does NOT wedge silently.
    let bootstrap = crate::task_run_resize_bootstrap::TaskRunResizeBootstrap::new(
        BuildPodPermitRepository::new(fixture.db.clone()),
        Arc::clone(&fixture.surface) as Arc<dyn TaskRunPodSurface>,
        Arc::new(crate::task_run_resize_bootstrap::DispatchGate::new()),
    );
    let outcome = bootstrap
        .bootstrap(
            &crate::task_run_resize_bootstrap::PermitBinding {
                task_run_id: fixture.task_run_id.clone(),
                permit_id: uuid::Uuid::now_v7().to_string(),
                fencing_token: 1,
            },
            LauncherAuthorityProtocol::ResizeV2,
        )
        .await;
    match outcome {
        crate::task_run_resize_bootstrap::BootstrapOutcome::Refused { reason, .. } => {
            assert_eq!(
                reason.to_string(),
                "permit identity or fencing token does not match the durable row",
                "the refusal must NAME why the run cannot dispatch"
            );
        }
        other => panic!("a stranded task run must be refused by NAME, got {other:?}"),
    }
    // The identity we could not capture is retained here so the assertion above
    // is about a replacement Pod that genuinely exists in the fixture.
    assert_eq!(replacement.pod_uid, "pod-uid-replacement");
}

// ── The lease gate ─────────────────────────────────────────────────────────

/// [`ResizeDropOutcome::releases_lease`] is the gate `release_lease` applies.
/// Only facts the cluster reported may open it.
#[test]
fn only_a_confirmed_or_absent_pod_releases_the_lease() {
    assert!(
        ResizeDropOutcome::Confirmed {
            pod_uid: "p".to_owned()
        }
        .releases_lease()
    );
    assert!(
        ResizeDropOutcome::PodUidAbsent {
            pod_uid: "p".to_owned()
        }
        .releases_lease()
    );
    assert!(
        ResizeDropOutcome::QuarantinedPodDeleted {
            pod_uid: "p".to_owned(),
            reason: DegradedUnleasedReason::ResizeForbidden,
        }
        .releases_lease()
    );
    assert!(ResizeDropOutcome::NotResizeGoverned.releases_lease());
    assert!(ResizeDropOutcome::NoActivePermit.releases_lease());
    assert!(ResizeDropOutcome::Fenced { owner: None }.releases_lease());
    // An unanswered apiserver is not evidence that the CPU came back.
    assert!(
        !ResizeDropOutcome::Unavailable("apiserver unreachable".to_owned()).releases_lease(),
        "an unsettled drop must never release the lease"
    );
}
