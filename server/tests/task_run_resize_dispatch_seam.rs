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
    BuildPodPermitRepository, BuildPodPermitState, CreateTaskRunParams, Database,
    EffectiveCreatorProvenance, ProjectRepository, TaskRepository, TaskRunRepository,
    UserRepository,
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
}

impl RecordingRuntime {
    fn new(job_uid: Option<&str>, protocol: Option<LauncherAuthorityProtocol>) -> Arc<Self> {
        Arc::new(Self {
            job_uid: job_uid.map(ToOwned::to_owned),
            protocol,
            attaches: Arc::new(AtomicUsize::new(0)),
            teardowns: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn attaches(&self) -> usize {
        self.attaches.load(Ordering::SeqCst)
    }

    fn teardowns(&self) -> usize {
        self.teardowns.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SessionRuntime for RecordingRuntime {
    async fn prepare(
        &self,
        spec: &TaskRunSpec,
        _credentials: &ResolvedCredentials,
    ) -> Result<RunHandle, RuntimeError> {
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

async fn seed(db: &Database, suffix: &str) -> Seeded {
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
    let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
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
    let spec = TaskRunSpec {
        task_run_id,
        task_attempt_id: None,
        task_id: task.id.clone(),
        project_id: project.id.clone(),
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
    };
    Seeded { task, spec }
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
