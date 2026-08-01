//! Reachability of proposal `3i92`'s resize stack from the **production
//! dispatch seam**.
//!
//! # Why these tests enter where they do
//!
//! Every test below calls
//! [`djinn_agent::actors::slot::supervisor_runner::execute_runtime_report_phase`]
//! — the function the slot actor calls in production, unchanged, with no test
//! shim in front of it. That entry point is the whole point of the file.
//!
//! This neighbourhood was burned repeatedly by work that was merged, green and
//! inert: a projection with no reader, a launcher path unreachable from its own
//! binary, a trait override nothing composed. Each of those had passing tests
//! that constructed the type under test directly and called its methods.
//! Constructing `TaskRunResizeBootstrap` and calling `bootstrap()` proves the
//! bootstrap works; it says nothing at all about whether anything calls it. So
//! these tests refuse that shortcut on purpose.
//!
//! # What is real here and what is not
//!
//! * **Postgres is real.** `Database::ephemeral()` is a template-cloned real
//!   Postgres, and every permit write goes through the real
//!   `BuildPodPermitRepository` against the real migration-162/164 schema,
//!   constraints and triggers included.
//! * **The composition is real.** The `AgentContext` comes from
//!   `AppState::agent_context()`, and the admission bridge is the real
//!   `TaskRunResizeAdmissionBridge` wrapping the real `TaskRunResizeBootstrap`
//!   and the real `DispatchGate`.
//! * **The resize client is real.** [`StoredTaskRunPod`] holds an actual `Pod`
//!   and drives `PodResizeClient` — so the birth downsize is confirmed by the
//!   real `confirm_launcher_cpu`, reading the real
//!   `status.initContainerStatuses`, comparing millicores through the real
//!   `CpuLimit`. The fixture lives in `djinn-k8s` because `deny.toml` bans a
//!   direct `k8s-openapi` dependency outside the Kubernetes capability owner.
//! * **Only the apiserver transport is substituted.** There is no fake
//!   repository anywhere: the thing under test is the permit relation and the
//!   confirmation rule, and both of those are the genuine article.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use djinn_core::clock::{Clock, SystemClock};
use djinn_db::{
    BuildPodPermitRepository, BuildPodPermitState, CreateTaskAttemptParams, CreateTaskRunParams,
    Database, EffectiveCreatorProvenance, ProjectRepository, TaskAttemptRepository, TaskRepository,
    TaskRunRepository, UserRepository,
};
use djinn_k8s::pod_resize::{CpuLimit, PodResizeError};
use djinn_k8s::pod_resize_fixture::StoredTaskRunPod;
use djinn_k8s::runtime::{LauncherObservationError, ObservedLauncherSidecar};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_runtime::{
    BiStream, ResolvedCredentials, RunHandle, RuntimeError, SessionRuntime, SupervisorFlow,
    TaskRunOutcome, TaskRunReport, TaskRunSpec,
};
use djinn_server::task_run_resize_bootstrap::{
    TaskRunPodSurface, TaskRunResizeAdmissionBridge, TaskRunResizeBootstrap,
};
use tokio_util::sync::CancellationToken;

/// The launcher CPU limit the Job manifest was rendered with.
const RENDERED_CEILING: &str = "4";

/// The birth limit every `resize-v2` launcher is downsized to before dispatch.
const BIRTH_MILLICORES: u64 = 250;

/// Wait budget for tests that expect confirmation. Short, because a healthy
/// fixture confirms on the first pass.
const CONFIRMING_BUDGET: Duration = Duration::from_secs(5);

/// Wait budget for tests that expect the seam to give up. Zero seconds is not
/// allowed by the env parser and would be a different code path anyway; one
/// pass then a deadline is exactly the "never becomes admitted" shape.
const GIVE_UP_BUDGET: Duration = Duration::from_millis(1);

// ── The apiserver surface ─────────────────────────────────────────────────

/// Adapts [`StoredTaskRunPod`] to the bootstrap's own surface trait.
///
/// Deliberately thin: every decision it could make is made inside the fixture,
/// which makes them through the production observation, resize and confirmation
/// functions. There is nothing here for a bug to hide in.
#[derive(Clone)]
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

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        self.0.uid_fenced_delete(task_run_id, pod_uid)
    }
}

// ── The fixture runtime ────────────────────────────────────────────────────

/// A `SessionRuntime` that creates nothing and records everything.
///
/// It exists so the seam's *ordering* is observable: `attaches` is the count of
/// worker sessions that actually went live, and a refused dispatch must leave it
/// at zero.
struct RecordingRuntime {
    job_uid: Option<String>,
    protocol: Option<LauncherAuthorityProtocol>,
    attaches: Arc<AtomicUsize>,
    teardowns: Arc<AtomicUsize>,
    /// When set, `prepare` reads the durable state of this dispatch **at the
    /// instant of the Job POST** and parks it in `at_job_post`.
    probe: Option<Database>,
    at_job_post: Arc<std::sync::Mutex<Option<DurableStateAtJobPost>>>,
}

/// What the database held for one dispatch at the moment its Job was POSTed.
///
/// Both counts are ordering facts, not liveness facts: the Job POST is the
/// point after which a Pod can exist, so anything that has to fence that Pod
/// must already be durable here.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DurableStateAtJobPost {
    task_run_status: Option<String>,
    build_pod_permits: i64,
}

impl RecordingRuntime {
    fn new(job_uid: Option<&str>, protocol: Option<LauncherAuthorityProtocol>) -> Arc<Self> {
        Arc::new(Self {
            job_uid: job_uid.map(ToOwned::to_owned),
            protocol,
            attaches: Arc::new(AtomicUsize::new(0)),
            teardowns: Arc::new(AtomicUsize::new(0)),
            probe: None,
            at_job_post: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Same runtime, but it reads the durable ordering out of `db` inside
    /// `prepare`. Asserting after the seam returns cannot distinguish "created
    /// before the Job" from "created after it"; this can.
    fn observing(
        db: &Database,
        job_uid: Option<&str>,
        protocol: Option<LauncherAuthorityProtocol>,
    ) -> Arc<Self> {
        Arc::new(Self {
            job_uid: job_uid.map(ToOwned::to_owned),
            protocol,
            attaches: Arc::new(AtomicUsize::new(0)),
            teardowns: Arc::new(AtomicUsize::new(0)),
            probe: Some(db.clone()),
            at_job_post: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    fn attaches(&self) -> usize {
        self.attaches.load(Ordering::SeqCst)
    }

    fn teardowns(&self) -> usize {
        self.teardowns.load(Ordering::SeqCst)
    }

    fn durable_state_at_job_post(&self) -> DurableStateAtJobPost {
        self.at_job_post
            .lock()
            .expect("job-post observation mutex")
            .clone()
            .expect("prepare must have run and recorded the durable state")
    }
}

#[async_trait]
impl SessionRuntime for RecordingRuntime {
    async fn prepare(
        &self,
        spec: &TaskRunSpec,
        _credentials: &ResolvedCredentials,
    ) -> Result<RunHandle, RuntimeError> {
        if let Some(db) = self.probe.as_ref() {
            db.ensure_initialized().await.expect("probe db");
            let task_run_status: Option<String> =
                sqlx::query_scalar("SELECT status FROM task_runs WHERE id = $1")
                    .bind(&spec.task_run_id)
                    .fetch_optional(db.pool())
                    .await
                    .expect("read task_runs at job post");
            let build_pod_permits: i64 = sqlx::query_scalar(
                "SELECT count(*)::bigint FROM build_pod_permits WHERE task_run_id = $1",
            )
            .bind(&spec.task_run_id)
            .fetch_one(db.pool())
            .await
            .expect("count build_pod_permits at job post");
            *self.at_job_post.lock().expect("job-post observation mutex") =
                Some(DurableStateAtJobPost {
                    task_run_status,
                    build_pod_permits,
                });
        }
        Ok(RunHandle {
            task_run_id: spec.task_run_id.clone(),
            container_id: None,
            pod_ref: Some("taskrun-fixture".to_owned()),
            started_at: SystemClock::new().now(),
            job_uid: self.job_uid.clone(),
            launcher_authority_protocol: self.protocol,
        })
    }

    async fn attach_stdio(&self, _handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        self.attaches.fetch_add(1, Ordering::SeqCst);
        // The fixture has no worker to talk to. What matters is that the call
        // happened at all — reaching this line is what "a shell can now run"
        // means at this seam.
        Err(RuntimeError::Attach(
            "fixture runtime has no worker session".to_owned(),
        ))
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        self.teardowns.fetch_add(1, Ordering::SeqCst);
        Ok(TaskRunReport {
            task_run_id: handle.task_run_id,
            outcome: TaskRunOutcome::Closed {
                reason: "fixture teardown".to_owned(),
            },
            stages_completed: Vec::new(),
        })
    }
}

// ── Durable seeding ────────────────────────────────────────────────────────

struct Seeded {
    task: djinn_core::models::Task,
    spec: TaskRunSpec,
}

/// Seed only the rows that exist before dispatch begins: a user, a project and
/// a task.
async fn seed_task(db: &Database, suffix: &str) -> djinn_core::models::Task {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            i64::try_from(uuid::Uuid::now_v7().as_u128() % 8_000_000_000_000_000_000)
                .expect("github id"),
            &format!("resize-seam-{suffix}-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed user");
    let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create(
            &format!("resize-seam-{suffix}"),
            "djinnos",
            &format!("resize-seam-{suffix}"),
        )
        .await
        .expect("seed project");
    TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_in_project_with_provenance(
            &project.id,
            None,
            EffectiveCreatorProvenance::explicit_user_id(&user.id),
            "resize seam",
            "description",
            "design",
            "task",
            2,
            "owner",
            None,
            None,
        )
        .await
        .expect("seed task")
}

/// A run that already has its durable `task_runs` row when the seam is entered.
///
/// This is **not** the shape production dispatch presents — see
/// [`seed_as_dispatch_does`] — but it is the right fixture for every assertion
/// about what the resize stack does once a permit can be acquired at all.
async fn seed(db: &Database, suffix: &str) -> Seeded {
    let task = seed_task(db, suffix).await;
    let task_run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &task_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed task run");
    Seeded {
        spec: spec_for(&task, task_run_id, None),
        task,
    }
}

/// The shape production dispatch actually presents to the seam.
///
/// `dispatch_task_runtime` mints a fresh uuid-v7 `task_run_id`, allocates the
/// `task_attempts` row for it, and enters `execute_runtime_report_phase`. It
/// does **not** create the `task_runs` row: until this fix, nothing on the host
/// did, and the only production creator was the in-pod supervisor's
/// `create_task_run` RPC — which cannot land until a Pod boot later.
///
/// Every other fixture in this file pre-seeds that row, which is precisely why
/// the whole file stayed green while every `resize-v2` dispatch in production
/// failed closed on a foreign-key violation it could not report.
async fn seed_as_dispatch_does(db: &Database, suffix: &str) -> Seeded {
    let task = seed_task(db, suffix).await;
    let task_run_id = uuid::Uuid::now_v7().to_string();
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("task-run:{task_run_id}");
    let attempt = TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &dispatch_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed the exact attempt dispatch allocates");
    Seeded {
        spec: spec_for(&task, task_run_id, Some(attempt.id)),
        task,
    }
}

fn spec_for(
    task: &djinn_core::models::Task,
    task_run_id: String,
    task_attempt_id: Option<String>,
) -> TaskRunSpec {
    TaskRunSpec {
        task_run_id,
        task_attempt_id,
        task_id: task.id.clone(),
        project_id: task.project_id.clone(),
        trigger: djinn_core::models::TaskRunTrigger::NewTask,
        base_branch: "main".to_owned(),
        task_branch: "djinn/resize-seam".to_owned(),
        flow: SupervisorFlow::NewTask,
        model_id_per_role: Default::default(),
        read_source_project_ids: Vec::new(),
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    }
}

/// The production `AgentContext` — built by `AppState::agent_context()`, the
/// real composition site — with only its apiserver surface substituted.
fn context_with(
    db: &Database,
    bridge: &Arc<TaskRunResizeAdmissionBridge>,
) -> djinn_agent::context::AgentContext {
    let mut context =
        djinn_server::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    assert!(
        context.resize_admission.is_some(),
        "AppState::agent_context() must already compose a resize admission bridge; \
         if this fires, the production composition site has regressed and every \
         other assertion in this file is measuring the fixture instead of the seam"
    );
    context.resize_admission = Some(bridge.clone());
    context
}

fn bridge_for(
    db: &Database,
    cluster: &StoredTaskRunPod,
    budget: Duration,
) -> Arc<TaskRunResizeAdmissionBridge> {
    Arc::new(
        TaskRunResizeAdmissionBridge::with_surface(
            db.clone(),
            Arc::new(FixtureSurface(cluster.clone())) as Arc<dyn TaskRunPodSurface>,
        )
        .with_wait(budget, Duration::from_millis(5)),
    )
}

async fn drive_seam(
    runtime: Arc<RecordingRuntime>,
    seeded: &Seeded,
    context: &djinn_agent::context::AgentContext,
) -> anyhow::Result<()> {
    djinn_agent::actors::slot::supervisor_runner::execute_runtime_report_phase(
        runtime,
        &seeded.spec,
        &ResolvedCredentials::default(),
        &seeded.task,
        "anthropic/claude-fixture",
        context,
        &CancellationToken::new(),
    )
    .await
    .map(|_| ())
}

// ── AC1 / AC6: the seam writes a fenced permit row for the observed Pod ────

/// **AC1 and AC6.** Drive the production dispatch seam for a `resize-v2` run
/// and read the durable row back.
///
/// What stays green if the body does nothing? Nothing. The row asserted here
/// only exists if `execute_runtime_report_phase` called
/// `BuildPodPermitRepository::acquire`; it only reaches `job_created` if the
/// seam called `bind_or_refresh_job_uid`; and it only reaches `birth_confirmed`
/// with an identity if the bootstrap ran `capture_resize_identity`. Delete the
/// `acquire` call from the seam and this test fails on the very first assertion
/// — there is no row at all.
#[tokio::test]
async fn the_production_dispatch_seam_writes_a_birth_confirmed_permit_for_the_observed_pod() {
    const POD_UID: &str = "pod-uid-ac1";
    const JOB_UID: &str = "job-uid-ac1";

    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed(&db, "ac1").await;
    let cluster = StoredTaskRunPod::resize_v2(POD_UID, RENDERED_CEILING);
    let bridge = bridge_for(&db, &cluster, CONFIRMING_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime = RecordingRuntime::new(Some(JOB_UID), Some(LauncherAuthorityProtocol::ResizeV2));

    drive_seam(runtime.clone(), &seeded, &context)
        .await
        .expect("a healthy resize-v2 dispatch is admitted");

    let row = BuildPodPermitRepository::new(db.clone())
        .active(&seeded.spec.task_run_id)
        .await
        .expect("read permit")
        .expect("the seam must have acquired a permit for this dispatch");
    assert_eq!(row.state, BuildPodPermitState::BirthConfirmed);
    assert_eq!(row.job_uid.as_deref(), Some(JOB_UID));

    let identity = row
        .resize_identity
        .expect("a birth_confirmed row carries a captured resize identity");
    // AC6: the Pod UID is OBSERVED, not inferred from the Job. Feed the capture
    // the Job UID instead and this pair of assertions fails.
    assert_eq!(identity.pod_uid, POD_UID);
    assert_ne!(
        identity.pod_uid, JOB_UID,
        "the captured fence must be the Pod's own uid, never the Job's"
    );
    assert_eq!(identity.effective_launcher_protocol, "resize-v2");
    assert_eq!(identity.admitted_cpu_millicores, 4000);
    assert_eq!(runtime.attaches(), 1, "a confirmed run does dispatch");
    assert_eq!(
        bridge.gate().dispatches_before_birth_confirmation(),
        0,
        "the confirmed run passed the gate before it attached"
    );
}

// ── AC3: the enforced limit, not a stored number ──────────────────────────

/// **AC3.** The birth limit is asserted from
/// `status.initContainerStatuses[cgroup-launcher].resources.limits.cpu` — the
/// field the kubelet writes when it actuates — parsed through `CpuLimit`.
///
/// What stays green if the body does nothing? Not this. Make the bootstrap
/// capture and store `admitted_cpu_millicores` but skip the resize call, and the
/// status below still reads the rendered ceiling: 4000m, not 250m. A test that
/// read `admitted_cpu_millicores` back out of the permit row would sail straight
/// through that mutation, which is why this test does not read it.
#[tokio::test]
async fn the_birth_limit_is_read_back_from_the_launcher_init_container_status() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed(&db, "ac3").await;
    let cluster = StoredTaskRunPod::resize_v2("pod-uid-ac3", RENDERED_CEILING);
    let bridge = bridge_for(&db, &cluster, CONFIRMING_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime = RecordingRuntime::new(
        Some("job-uid-ac3"),
        Some(LauncherAuthorityProtocol::ResizeV2),
    );

    // The ceiling is real and is not the birth limit, so "already there" cannot
    // be mistaken for "was downsized".
    assert_eq!(
        CpuLimit::parse(RENDERED_CEILING)
            .expect("ceiling parses")
            .millis(),
        4000
    );

    drive_seam(runtime, &seeded, &context)
        .await
        .expect("a healthy resize-v2 dispatch is admitted");

    let observed = cluster
        .launcher_status_cpu()
        .expect("the launcher's init-container status reports a cpu limit");
    assert_eq!(
        CpuLimit::parse(&observed)
            .expect("the reported quantity parses")
            .millis(),
        BIRTH_MILLICORES,
        "the launcher must actually be at the birth limit, not merely recorded as such"
    );
    assert_eq!(cluster.resize_patches(), 1);

    // The rest of the Pod is untouched: a limits-only resize must not move
    // requests (that would change scheduling and Kueue accounting) and must not
    // drop the other init container (that would be an RFC 7386 merge).
    assert_eq!(
        cluster.init_container_count(),
        2,
        "a strategic merge keeps every other init container; a merge patch would drop them"
    );
    assert_eq!(
        cluster.launcher_spec_cpu_request().as_deref(),
        Some("100m"),
        "requests are byte-identical across the resize"
    );
    assert_eq!(
        cluster.qos_class().as_deref(),
        Some("Burstable"),
        "the QoS class is byte-identical across the resize"
    );
}

// ── AC4 reachability: the status fence stands on the live path ────────────

/// **AC4, from the seam.** `crates/djinn-k8s/tests/pod_resize_serialization.rs`
/// already proves `confirm_launcher_cpu` refuses a Pod whose *regular*
/// container status matches the target, refuses a `PodResizePending` Pod, and
/// confirms `4` against a target of `4000m`. Those are properties of a function.
/// This proves the function is on the dispatch path: a Pod whose launcher is
/// accepted but never actuated must not dispatch.
///
/// Point confirmation at `status.containerStatuses` and the misleading worker
/// entry the fixture Pod carries — `cpu: 4`, never resized — is what it would
/// read.
#[tokio::test]
async fn an_accepted_but_unactuated_resize_never_dispatches() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed(&db, "ac4").await;
    // The apiserver accepts the PATCH; the kubelet never moves the status.
    let cluster = StoredTaskRunPod::resize_v2("pod-uid-ac4", RENDERED_CEILING).never_actuating();
    let bridge = bridge_for(&db, &cluster, GIVE_UP_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime = RecordingRuntime::new(
        Some("job-uid-ac4"),
        Some(LauncherAuthorityProtocol::ResizeV2),
    );

    let error = drive_seam(runtime.clone(), &seeded, &context)
        .await
        .expect_err("an unconfirmed birth downsize must not dispatch");

    assert!(
        cluster.resize_patches() >= 1,
        "the PATCH was issued and accepted; only confirmation failed"
    );
    assert_eq!(
        runtime.attaches(),
        0,
        "no worker session may start behind an unconfirmed birth limit: {error:#}"
    );
    assert_eq!(runtime.teardowns(), 1, "the refused run is torn down");
}

// ── AC5: the gate is load-bearing, and its counter is not structurally zero ─

/// **AC5.** A `resize-v2` Pod whose launcher never starts — no `containerID`, so
/// no identity can be fenced — must exhaust the wait budget and fail closed:
/// nothing captured, nothing dispatched, and the Pod UID-fenced deleted.
///
/// Change the seam's refusal arm to log-and-continue and the `attaches()`
/// assertion below fails immediately.
#[tokio::test]
async fn a_launcher_that_never_starts_fails_closed_and_the_pod_is_uid_fenced_deleted() {
    const POD_UID: &str = "pod-uid-ac5";

    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed(&db, "ac5").await;
    // Admitted and nameable in both arrays, but the kubelet has not started it:
    // no containerID, so there is nothing to fence a write-once identity with.
    let cluster = StoredTaskRunPod::resize_v2_launcher_not_started(POD_UID, RENDERED_CEILING);
    let bridge = bridge_for(&db, &cluster, GIVE_UP_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime = RecordingRuntime::new(
        Some("job-uid-ac5"),
        Some(LauncherAuthorityProtocol::ResizeV2),
    );

    let error = drive_seam(runtime.clone(), &seeded, &context)
        .await
        .expect_err("a launcher that never starts must not dispatch");

    assert_eq!(
        runtime.attaches(),
        0,
        "zero worker sessions started: {error:#}"
    );
    assert_eq!(cluster.resize_patches(), 0, "nothing was resized");
    assert_eq!(
        cluster.deletes(),
        vec![(seeded.spec.task_run_id.clone(), POD_UID.to_owned())],
        "the ungovernable Pod is destroyed, fenced to its own uid"
    );
    let row = BuildPodPermitRepository::new(db.clone())
        .active(&seeded.spec.task_run_id)
        .await
        .expect("read permit")
        .expect("the permit row still exists");
    assert!(
        row.resize_identity.is_none(),
        "no identity may be captured for a launcher that never started"
    );
    assert_eq!(row.state, BuildPodPermitState::JobCreated);
    assert_eq!(
        bridge.gate().dispatches_before_birth_confirmation(),
        0,
        "nothing dispatched, so nothing dispatched unadmitted"
    );
}

/// **AC5, non-vacuity.** The unadmitted-dispatch counter must be capable of
/// moving. On `main` it is zero because nothing calls it at all; that is the
/// failure mode this asserts against.
///
/// A dispatch that renders no launcher authority never passes through the birth
/// gate — correctly, there is no launcher quota to establish — but it still
/// reaches the dispatch site. The counter therefore climbs, which is exactly
/// what it would do if the gate above were deleted.
#[tokio::test]
async fn the_unadmitted_dispatch_counter_moves_when_a_dispatch_bypasses_the_gate() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed(&db, "ac5b").await;
    let cluster = StoredTaskRunPod::resize_v2("pod-uid-ac5b", RENDERED_CEILING);
    let bridge = bridge_for(&db, &cluster, CONFIRMING_BUDGET);
    let context = context_with(&db, &bridge);
    // No Job, no launcher, no protocol — the shape every non-Kubernetes runtime
    // produces, and the shape a deleted gate would produce for every run.
    let runtime = RecordingRuntime::new(None, None);

    assert_eq!(bridge.gate().dispatches_before_birth_confirmation(), 0);
    let _ = drive_seam(runtime.clone(), &seeded, &context).await;

    assert_eq!(runtime.attaches(), 1, "the dispatch site was reached");
    assert_eq!(
        bridge.gate().dispatches_before_birth_confirmation(),
        1,
        "record_dispatch_started is wired to the real dispatch site and its counter is live"
    );
}

// ── AC7: leaf-v1 is untouched ─────────────────────────────────────────────

/// **AC7.** A `leaf-v1` dispatch issues zero `pods/resize` PATCHes, captures no
/// resize identity, and still dispatches.
///
/// Remove the protocol branch so the bootstrap runs unconditionally and the
/// PATCH counter below is no longer zero.
#[tokio::test]
async fn a_leaf_v1_dispatch_issues_no_resize_and_captures_no_identity() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed(&db, "ac7").await;
    // `resolve_launcher_cpu_ceiling` returns `Ok(None)` for leaf-v1, so a
    // leaf-v1 Pod carries no launcher CPU limit at all. A limit there would be an
    // ancestor clamp over every invocation leaf.
    let cluster = StoredTaskRunPod::leaf_v1("pod-uid-ac7");
    let bridge = bridge_for(&db, &cluster, CONFIRMING_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime =
        RecordingRuntime::new(Some("job-uid-ac7"), Some(LauncherAuthorityProtocol::LeafV1));

    drive_seam(runtime.clone(), &seeded, &context)
        .await
        .expect("a leaf-v1 dispatch is unaffected by the resize stack");

    assert_eq!(
        cluster.resize_patches(),
        0,
        "leaf-v1 must never touch pods/resize"
    );
    assert_eq!(
        cluster.deletes(),
        Vec::new(),
        "leaf-v1 Pods are not deleted"
    );
    assert_eq!(
        runtime.attaches(),
        1,
        "leaf-v1 dispatches exactly as before"
    );
    let row = BuildPodPermitRepository::new(db.clone())
        .active(&seeded.spec.task_run_id)
        .await
        .expect("read permit")
        .expect("permit row");
    assert!(
        row.resize_identity.is_none(),
        "leaf-v1 has no ceiling to capture and migration 164 would reject one"
    );
    assert_eq!(row.state, BuildPodPermitState::JobCreated);
    assert_eq!(bridge.gate().dispatches_before_birth_confirmation(), 0);
}

// ── The dispatch-time ordering of `task_runs` and `build_pod_permits` ──────

/// **The regression.** A dispatch entering the seam the way production enters
/// it — fresh uuid-v7 run id, `task_attempts` row allocated, **no `task_runs`
/// row yet** — must still end up with a durable permit row.
///
/// `build_pod_permits.task_run_id` is `REFERENCES task_runs(id)`. Before this
/// fix nothing on the host created that parent, so `acquire` violated the
/// foreign key, `BuildPodPermitRepository::acquire` collapsed the violation into
/// `Unavailable` to fail closed, and `admit_task_run_dispatch` refused every
/// `resize-v2` render with "holds no durable build-pod permit". Production had
/// zero rows in `build_pod_permits`.
///
/// What stays green if the body does nothing? Nothing. This asserts the ROW, in
/// the real table, behind the real foreign key. Delete the
/// `ensure_durable_task_run_row` call from `execute_runtime_report_phase` and
/// the permit lookup below returns `None` — there is no row at all, exactly as
/// in production.
#[tokio::test]
async fn the_seam_acquires_a_permit_for_a_dispatch_that_has_no_task_runs_row_yet() {
    const POD_UID: &str = "pod-uid-fk";
    const JOB_UID: &str = "job-uid-fk";

    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed_as_dispatch_does(&db, "fk").await;

    // The premise of the whole test: production's parent row is genuinely absent
    // when the seam is entered. If a future refactor makes some earlier step
    // create it, this assertion fails rather than letting the test go vacuous.
    let before: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM task_runs WHERE id = $1")
        .bind(&seeded.spec.task_run_id)
        .fetch_one(db.pool())
        .await
        .expect("count task_runs before dispatch");
    assert_eq!(
        before, 0,
        "production enters this seam with no task_runs row; a fixture that pre-seeds one \
         is measuring itself"
    );

    let cluster = StoredTaskRunPod::resize_v2(POD_UID, RENDERED_CEILING);
    let bridge = bridge_for(&db, &cluster, CONFIRMING_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime = RecordingRuntime::new(Some(JOB_UID), Some(LauncherAuthorityProtocol::ResizeV2));

    drive_seam(runtime.clone(), &seeded, &context)
        .await
        .expect("a resize-v2 dispatch with no pre-existing task_runs row is admitted");

    let row = BuildPodPermitRepository::new(db.clone())
        .active(&seeded.spec.task_run_id)
        .await
        .expect("read permit")
        .expect(
            "the seam must have acquired a permit; a None here is the production failure — \
             the foreign key onto task_runs had no parent to point at",
        );
    assert_eq!(row.state, BuildPodPermitState::BirthConfirmed);
    assert_eq!(row.job_uid.as_deref(), Some(JOB_UID));
    assert_eq!(runtime.attaches(), 1, "the worker session started");

    // The parent the foreign key points at is the run's own row, not some other
    // dispatch's, and the host terminalizes the row it created rather than
    // leaving it live for the stale sweep.
    let (task_id, status): (String, String) =
        sqlx::query_as("SELECT task_id, status FROM task_runs WHERE id = $1")
            .bind(&seeded.spec.task_run_id)
            .fetch_one(db.pool())
            .await
            .expect("the durable task_runs row exists after dispatch");
    assert_eq!(task_id, seeded.task.id);
    assert!(
        matches!(status.as_str(), "completed" | "failed" | "interrupted"),
        "the host reaps the run row it created; found {status}"
    );
}

/// **The ordering.** Both durable rows must exist at the instant the Job is
/// POSTed, because the Job POST is the point after which a Pod can exist.
///
/// Asserting after the seam returns proves only that the rows were written at
/// some point. This reads them from inside `SessionRuntime::prepare`, which is
/// the Job POST itself, so it can tell "before" from "after".
///
/// Two named mutations make it red, and they are the two orderings that matter:
///
/// * move `ensure_durable_task_run_row` to after `runtime.prepare` — the
///   `task_runs` count at the Job POST is 0, and the permit is gone with it;
/// * move `acquire_build_pod_permit` to after `runtime.prepare` — the permit
///   count at the Job POST is 0, which is the fencing violation the permit's
///   own doc comment forbids.
#[tokio::test]
async fn both_durable_rows_exist_before_the_job_that_they_fence_is_posted() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed_as_dispatch_does(&db, "order").await;
    let cluster = StoredTaskRunPod::resize_v2("pod-uid-order", RENDERED_CEILING);
    let bridge = bridge_for(&db, &cluster, CONFIRMING_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime = RecordingRuntime::observing(
        &db,
        Some("job-uid-order"),
        Some(LauncherAuthorityProtocol::ResizeV2),
    );

    drive_seam(runtime.clone(), &seeded, &context)
        .await
        .expect("a healthy resize-v2 dispatch is admitted");

    assert_eq!(
        runtime.durable_state_at_job_post(),
        DurableStateAtJobPost {
            // `starting`, not merely present: the host creates the row in the
            // same pre-session state the in-pod supervisor would have.
            task_run_status: Some("starting".to_owned()),
            build_pod_permits: 1,
        },
        "the task_runs row and the permit that references it must both be durable \
         before the Job POST, not after it"
    );
}

/// **The guard is not weakened.** With the ordering fixed, a `resize-v2` render
/// that genuinely cannot obtain a permit must still refuse to start a worker
/// session.
///
/// Two components writing one leaf's `cpu.max`, or none, is the exact hazard
/// `3i92` exists to prevent, so "no permit" must stay fatal for `resize-v2`. The
/// permit is made genuinely unobtainable by removing the global pool row that
/// `acquire` locks first — a real repository failure, not a stubbed result.
///
/// Change `admit_task_run_dispatch`'s `permit.is_none()` arm to return `Ok(())`
/// unconditionally instead of bailing for non-leaf, and `attaches()` below goes
/// to 1.
#[tokio::test]
async fn a_resize_v2_render_still_refuses_when_no_permit_can_be_obtained() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let seeded = seed_as_dispatch_does(&db, "nopermit").await;
    BuildPodPermitRepository::new(db.clone())
        .delete_global_pool_for_test()
        .await
        .expect("remove the pool row acquire locks first");

    let cluster = StoredTaskRunPod::resize_v2("pod-uid-nopermit", RENDERED_CEILING);
    let bridge = bridge_for(&db, &cluster, CONFIRMING_BUDGET);
    let context = context_with(&db, &bridge);
    let runtime = RecordingRuntime::new(
        Some("job-uid-nopermit"),
        Some(LauncherAuthorityProtocol::ResizeV2),
    );

    let error = drive_seam(runtime.clone(), &seeded, &context)
        .await
        .expect_err("a resize-v2 render with no permit must not dispatch");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("holds no durable build-pod permit"),
        "the refusal must name the missing permit, not some downstream symptom: {rendered}"
    );

    assert_eq!(
        runtime.attaches(),
        0,
        "no worker session may start with no resize identity: {rendered}"
    );
    assert_eq!(runtime.teardowns(), 1, "the refused run is torn down");
    assert_eq!(cluster.resize_patches(), 0, "nothing was resized");
    assert!(
        BuildPodPermitRepository::new(db.clone())
            .active(&seeded.spec.task_run_id)
            .await
            .expect("read permit")
            .is_none(),
        "no permit was acquired, so the refusal is the real one and not a stubbed result"
    );

    // The run row the host created is terminalized rather than stranded
    // `starting` — before dispatch created it, this path had nothing to reap.
    let status: String = sqlx::query_scalar("SELECT status FROM task_runs WHERE id = $1")
        .bind(&seeded.spec.task_run_id)
        .fetch_one(db.pool())
        .await
        .expect("the durable task_runs row exists");
    assert_eq!(
        status, "failed",
        "a refused dispatch must not leave its run row live forever"
    );
}

/// The bootstrap type is still constructible and still composes — a compile-time
/// statement only, kept so a refactor that changes its shape is caught here
/// rather than in the bridge's private guts.
#[test]
fn the_bootstrap_type_is_the_one_the_bridge_composes() {
    fn _accepts(
        _: fn(
            BuildPodPermitRepository,
            Arc<dyn TaskRunPodSurface>,
            Arc<djinn_server::task_run_resize_bootstrap::DispatchGate>,
        ) -> TaskRunResizeBootstrap,
    ) {
    }
    _accepts(TaskRunResizeBootstrap::new);
}
