//! The startup census must not mistake a dispatch that is mid-CREATE for a
//! dispatch that died (task `ci29`, epic `43ww`, proposal `ih1w`).
//!
//! # Why this test enters where it does
//!
//! There is a real window in production between the moment
//! `execute_runtime_report_phase` commits a `task_runs` row with
//! `status = 'starting'` and the moment the Kubernetes Job for that run exists.
//! A server that boots inside that window LISTs the namespace, does not find
//! the Job, confirms its absence with an independent GET — and has, by every
//! rule the census knows, *positive* evidence that the run is gone. It is not.
//!
//! Pre-seeding a `starting` row and then running the census proves nothing
//! about that window, because a pre-seeded row is not inside it. So this test
//! blocks *in* the window: the `SessionRuntime` it injects is the production
//! seam's own launch verb, and the entire startup sequence — census
//! acquisition, Stage A, Stage B, Stage C — runs inside `prepare`, after the
//! durable `starting` commit and before the Job is created. When `prepare`
//! returns, CREATE commits and the rest of the dispatch proceeds, which is what
//! makes the preserved rows meaningful rather than merely untouched.
//!
//! # What is real here and what is not
//!
//! * **Postgres is real** — `Database::ephemeral()`, real repositories, real
//!   constraints.
//! * **The dispatch path is real** —
//!   `djinn_agent::actors::slot::supervisor_runner::execute_runtime_report_phase`,
//!   the function the slot actor calls in production, with no shim in front.
//!   It is what writes the `starting` row this test fences.
//! * **The startup sequence is real** — `StartupCensus::acquire`,
//!   `AppState::interrupt_stale_sessions_on_startup_with_census` (through the
//!   crate's own forwarding helper) and
//!   `djinn_coordinator::complete_startup_reaper_phase`.
//! * **Only the apiserver transport is substituted** — the namespace inventory
//!   and the launcher Pod surface. The Job CREATE itself is the injected
//!   runtime's `prepare`; what proves it committed is the durable
//!   `build_pod_permits` row the seam binds to the returned Job UID *after*
//!   `prepare` returns, which cannot exist unless the launch succeeded.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use djinn_coordinator::startup_census::{
    GoneProvenance, StartupCensus, TaskCensusProjection, TaskRunWitness,
};
use djinn_core::clock::{Clock, SystemClock};
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::test_support::backdate_task_attempt_created_at;
use djinn_db::{
    BuildPodPermitRepository, BuildPodPermitState, CreateTaskAttemptParams, CreateTaskRunParams,
    Database, EffectiveCreatorProvenance, ProjectRepository, SessionRepository,
    TaskAttemptRepository, TaskRepository, TaskRunRepository, UserRepository,
};
use djinn_k8s::pod_resize::PodResizeError;
use djinn_k8s::pod_resize_fixture::StoredTaskRunPod;
use djinn_k8s::runtime::{JobAdmission, LauncherObservationError, ObservedLauncherSidecar};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_runtime::{
    BiStream, ResolvedCredentials, RunHandle, RuntimeError, SessionRuntime, SupervisorFlow,
    TaskRunOutcome, TaskRunReport, TaskRunSpec,
};
use djinn_server::server::AppState;
use djinn_server::task_run_resize_bootstrap::{TaskRunPodSurface, TaskRunResizeAdmissionBridge};
use tokio_util::sync::CancellationToken;

const RENDERED_CEILING: &str = "4";
const POD_UID: &str = "pod-uid-create-transit";
const JOB_UID: &str = "job-uid-create-transit";
const CONFIRMING_BUDGET: Duration = Duration::from_secs(5);

// ── The apiserver surfaces ────────────────────────────────────────────────

/// Adapts [`StoredTaskRunPod`] to the resize bootstrap's surface trait.
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

    async fn observe_job_admission(&self, _task_run_id: &str) -> JobAdmission {
        self.0.job_admission()
    }

    async fn uid_fenced_delete(&self, task_run_id: &str, pod_uid: &str) -> Result<(), String> {
        self.0.uid_fenced_delete(task_run_id, pod_uid)
    }
}

/// A namespace inventory whose LIST is empty and whose per-object GET answers
/// from a fixed table. This is the shape a real apiserver presents while a Job
/// POST is still in flight: the object is not in the listing, and asking for it
/// by name says it is not there either.
struct AbsentInventory {
    presence: HashMap<String, ObjectPresence>,
}

#[async_trait]
impl WorkloadInventory for AbsentInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Ok(Vec::new())
    }

    async fn get_uid(&self, _: WorkloadObjectKind, _: &str, _: &str) -> UidGetResult {
        UidGetResult::Uncertain
    }

    async fn presence(&self, _: WorkloadObjectKind, name: &str) -> ObjectPresence {
        self.presence
            .get(name)
            .cloned()
            .unwrap_or(ObjectPresence::Uncertain)
    }
}

// ── What the startup sequence saw and did, from inside the window ─────────

/// Everything the fenced startup sequence observed at the CREATE boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FenceObservation {
    /// The durable status of the dispatching run when the barrier was reached.
    status_at_barrier: Option<String>,
    fenced_witness: Option<TaskRunWitness>,
    fenced_projection: Option<TaskCensusProjection>,
    control_witness: Option<TaskRunWitness>,
    control_projection: Option<TaskCensusProjection>,
    /// `(session, task run, attempt)` for the fenced dispatch after Stage A/B/C.
    fenced_after: (String, String, String),
    /// `(session, task run, attempt)` for the paired absent `running` control.
    control_after: (String, String, String),
}

/// The paired control: an ordinary `running` dispatch whose Job is absent for
/// exactly the same reason the fenced one's is — it is not there — but whose
/// durable state carries no commit-then-CREATE excuse.
#[derive(Clone)]
struct AbsentRunningControl {
    run_id: String,
    session_id: String,
    attempt_id: String,
}

/// A `SessionRuntime` that runs the whole production startup sequence at the
/// instant of the Job POST, then lets the POST proceed.
///
/// `prepare` is the launch verb `execute_runtime_report_phase` calls; entering
/// it means the durable `starting` commit has already happened and the Job does
/// not exist yet. Doing the census work here — rather than signalling a barrier
/// and racing another task — makes the window deterministic without a clock.
struct CreateTransitFence {
    db: Database,
    inventory: Arc<AbsentInventory>,
    control: AbsentRunningControl,
    observed: Mutex<Option<FenceObservation>>,
}

impl CreateTransitFence {
    fn observation(&self) -> FenceObservation {
        self.observed
            .lock()
            .expect("fence observation mutex")
            .clone()
            .expect("prepare must have run the fenced startup sequence")
    }
}

#[async_trait]
impl SessionRuntime for CreateTransitFence {
    async fn prepare(
        &self,
        spec: &TaskRunSpec,
        _credentials: &ResolvedCredentials,
    ) -> Result<RunHandle, RuntimeError> {
        let runs = TaskRunRepository::new(self.db.clone());
        let status_at_barrier = runs
            .get(&spec.task_run_id)
            .await
            .expect("read the dispatching run at the CREATE boundary")
            .map(|run| run.status);

        // The window's own linked session. A `starting` run is promoted to
        // `running` the moment a session is attached to it, so restore the
        // committed pre-CREATE state the barrier is standing in.
        let events = djinn_core::events::EventBus::noop();
        let sessions = SessionRepository::new(self.db.clone(), events.clone());
        let fenced_session = sessions
            .create(CreateSessionParams {
                project_id: &spec.project_id,
                task_id: Some(&spec.task_id),
                model: "openai/gpt-5.5",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: Some(&spec.task_run_id),
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("link a session to the run that is mid-CREATE")
            .id;
        runs.update_status(
            &spec.task_run_id,
            djinn_core::models::TaskRunStatus::Starting,
        )
        .await
        .expect("restore the committed pre-CREATE starting state");

        // ── The production startup sequence, entirely inside the window ──
        let census = StartupCensus::acquire(self.db.clone(), Some(self.inventory.clone()))
            .await
            .expect("acquire the immutable census while CREATE is blocked");
        let witness_of = |run_id: &str| {
            census
                .runs()
                .iter()
                .find(|run| run.task_run_id == run_id)
                .map(|run| run.witness)
        };
        let fenced_witness = witness_of(&spec.task_run_id);
        let control_witness = witness_of(&self.control.run_id);
        let fenced_projection = census.task_projection(&spec.task_id);
        let control_task_id = runs
            .get(&self.control.run_id)
            .await
            .expect("read control run")
            .expect("control run exists")
            .task_id;
        let control_projection = census.task_projection(&control_task_id);

        let state = AppState::new(self.db.clone(), CancellationToken::new());
        djinn_server::test_helpers::run_startup_stage_a(&state, &census).await;
        djinn_coordinator::complete_startup_reaper_phase(
            &self.db,
            "create-transit-fence-incarnation",
            Some(&census),
        )
        .await;

        let fenced_after = durable_triple(
            &self.db,
            &events,
            &fenced_session,
            &spec.task_run_id,
            spec.task_attempt_id
                .as_deref()
                .expect("dispatch allocates a task attempt"),
        )
        .await;
        let control_after = durable_triple(
            &self.db,
            &events,
            &self.control.session_id,
            &self.control.run_id,
            &self.control.attempt_id,
        )
        .await;

        *self.observed.lock().expect("fence observation mutex") = Some(FenceObservation {
            status_at_barrier,
            fenced_witness,
            fenced_projection,
            control_witness,
            control_projection,
            fenced_after,
            control_after,
        });

        // ── Barrier released: the Job POST proceeds ──
        Ok(RunHandle {
            task_run_id: spec.task_run_id.clone(),
            container_id: None,
            pod_ref: Some("taskrun-create-transit".to_owned()),
            started_at: SystemClock::new().now(),
            job_uid: Some(JOB_UID.to_owned()),
            launcher_authority_protocol: Some(LauncherAuthorityProtocol::ResizeV2),
        })
    }

    async fn attach_stdio(&self, _handle: &RunHandle) -> Result<BiStream, RuntimeError> {
        Err(RuntimeError::Attach(
            "fixture runtime has no worker session".to_owned(),
        ))
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn teardown(&self, handle: RunHandle) -> Result<TaskRunReport, RuntimeError> {
        Ok(TaskRunReport {
            task_run_id: handle.task_run_id,
            outcome: TaskRunOutcome::Closed {
                reason: "fixture teardown".to_owned(),
            },
            stages_completed: Vec::new(),
        })
    }
}

async fn durable_triple(
    db: &Database,
    events: &djinn_core::events::EventBus,
    session_id: &str,
    run_id: &str,
    attempt_id: &str,
) -> (String, String, String) {
    let session = SessionRepository::new(db.clone(), events.clone())
        .get(session_id)
        .await
        .expect("read linked session")
        .expect("linked session exists")
        .status;
    let run = TaskRunRepository::new(db.clone())
        .get(run_id)
        .await
        .expect("read linked task run")
        .expect("linked task run exists")
        .status;
    let attempt = TaskAttemptRepository::new(db.clone())
        .get(attempt_id)
        .await
        .expect("read linked attempt")
        .expect("linked attempt exists")
        .outcome;
    (session, run, attempt)
}

// ── Fixtures ──────────────────────────────────────────────────────────────

async fn seed_task(db: &Database, suffix: &str) -> djinn_core::models::Task {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(
            i64::try_from(uuid::Uuid::now_v7().as_u128() % 8_000_000_000_000_000_000)
                .expect("github id"),
            &format!("create-transit-{suffix}-{}", uuid::Uuid::now_v7()),
            None,
            None,
        )
        .await
        .expect("seed user");
    let project = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create(
            &format!("create-transit-{suffix}"),
            "djinnos",
            &format!("create-transit-{suffix}"),
        )
        .await
        .expect("seed project");
    TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create_in_project_with_provenance(
            &project.id,
            None,
            EffectiveCreatorProvenance::explicit_user_id(&user.id),
            "create transit",
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

/// The shape production dispatch presents to the seam: a fresh `task_run_id`
/// and its `task_attempts` row, and **no** `task_runs` row — the seam commits
/// that itself, which is the commit this test fences.
async fn seed_as_dispatch_does(db: &Database) -> (djinn_core::models::Task, TaskRunSpec) {
    let task = seed_task(db, "fenced").await;
    let task_run_id = uuid::Uuid::now_v7().to_string();
    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &format!("task-run:{task_run_id}"),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed the exact attempt dispatch allocates");
    // Stage C only considers attempts past its age gate. Without this the
    // fenced attempt would survive because it is young, not because it is
    // fenced — which would make the assertion vacuous.
    backdate_task_attempt_created_at(db, &attempt_id, "1 hour").await;
    let spec = TaskRunSpec {
        task_run_id,
        task_attempt_id: Some(attempt_id),
        task_id: task.id.clone(),
        execution_generation: 0,
        project_id: task.project_id.clone(),
        trigger: djinn_core::models::TaskRunTrigger::NewTask,
        base_branch: "main".to_owned(),
        task_branch: "djinn/create-transit".to_owned(),
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
    (task, spec)
}

/// A fully linked `running` dispatch whose Job is absent. Same census, same
/// evidence class, different durable state.
async fn seed_absent_running_control(db: &Database) -> AbsentRunningControl {
    let task = seed_task(db, "control").await;
    let run_id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed control task run");
    let session_id = SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("seed control session")
        .id;
    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &format!("task-run:{run_id}"),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed control attempt");
    backdate_task_attempt_created_at(db, &attempt_id, "1 hour").await;
    AbsentRunningControl {
        run_id,
        session_id,
        attempt_id,
    }
}

// ── The regression ────────────────────────────────────────────────────────

/// Drive the production dispatch seam, run the whole startup sequence at the
/// Job-POST boundary, and prove the fence is state-specific.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_starting_create_transit_is_fenced() {
    let db = Database::ephemeral().await.expect("ephemeral db");
    let (_task, spec) = seed_as_dispatch_does(&db).await;
    let control = seed_absent_running_control(&db).await;

    let mut presence = HashMap::new();
    presence.insert(
        djinn_k8s::taskrun_job_name(&spec.task_run_id),
        ObjectPresence::Absent,
    );
    presence.insert(
        djinn_k8s::taskrun_job_name(&control.run_id),
        ObjectPresence::Absent,
    );
    let inventory = Arc::new(AbsentInventory { presence });

    let fence = Arc::new(CreateTransitFence {
        db: db.clone(),
        inventory,
        control: control.clone(),
        observed: Mutex::new(None),
    });

    let cluster = StoredTaskRunPod::resize_v2(POD_UID, RENDERED_CEILING);
    let bridge = Arc::new(
        TaskRunResizeAdmissionBridge::with_surface(
            db.clone(),
            Arc::new(FixtureSurface(cluster)) as Arc<dyn TaskRunPodSurface>,
        )
        .with_wait(CONFIRMING_BUDGET, Duration::from_millis(5)),
    );
    let mut context =
        djinn_server::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    assert!(
        context.resize_admission.is_some(),
        "AppState::agent_context() must already compose a resize admission bridge"
    );
    context.resize_admission = Some(bridge.clone());

    let task = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .get(&spec.task_id)
        .await
        .expect("read dispatching task")
        .expect("dispatching task exists");

    djinn_agent::actors::slot::supervisor_runner::execute_runtime_report_phase(
        fence.clone(),
        &spec,
        &ResolvedCredentials::default(),
        &task,
        "anthropic/claude-fixture",
        &context,
        &CancellationToken::new(),
    )
    .await
    .expect("the fenced dispatch is admitted");

    let observed = fence.observation();

    // AC1: the barrier is after the durable `starting` commit and before CREATE.
    assert_eq!(
        observed.status_at_barrier.as_deref(),
        Some("starting"),
        "the barrier must sit after the durable starting commit"
    );

    // AC2: the census independently confirms absence and still fences the run.
    assert_eq!(
        observed.fenced_witness,
        Some(TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)),
        "an omitted Job confirmed absent by GET is authoritative absence"
    );
    assert_eq!(
        observed.fenced_projection,
        Some(TaskCensusProjection::CreationTransit),
        "authoritative pre-CREATE absence of a durable starting row is CreationTransit"
    );
    assert_eq!(
        observed.fenced_after,
        (
            "running".to_owned(),
            "starting".to_owned(),
            "pending".to_owned()
        ),
        "Stage A/B/C must leave the linked session, starting run and pending attempt alone"
    );

    // AC4: identical evidence, different durable state, opposite outcome.
    assert_eq!(
        observed.control_witness,
        Some(TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)),
        "the control carries exactly the same Gone provenance"
    );
    assert_eq!(
        observed.control_projection,
        Some(TaskCensusProjection::DestructivelyGone)
    );
    assert_eq!(
        observed.control_after,
        (
            "interrupted".to_owned(),
            "interrupted".to_owned(),
            "interrupted".to_owned()
        ),
        "the paired absent running dispatch is destructively reconciled, \
         so the fence is state-specific rather than preserve-all"
    );

    // AC3: the barrier released and the launch committed. This row is written
    // by the seam *after* `prepare` returned the Job UID, so it cannot exist
    // unless CREATE succeeded and the dispatch continued past it.
    let permit = BuildPodPermitRepository::new(db.clone())
        .active(&spec.task_run_id)
        .await
        .expect("read permit")
        .expect("the resumed dispatch must hold a permit");
    assert_eq!(permit.job_uid.as_deref(), Some(JOB_UID));
    assert_eq!(permit.state, BuildPodPermitState::BirthConfirmed);

    // And the durable rows survive the resumed dispatch, not just the census.
    assert_eq!(
        TaskRunRepository::new(db.clone())
            .get(&control.run_id)
            .await
            .expect("read control run after resume")
            .expect("control run exists")
            .status,
        "interrupted"
    );
}
