//! Deterministic test coverage for the startup reconnectability measurement
//! (proposal `phif` AC 7/8).
//!
//! This module verifies that the measurement seam in
//! [`AppState::measure_startup_reconnectability`] correctly counts running
//! sessions and their connected-worker status **before** the startup
//! blanket-interruption mutation runs.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::events::EventBus;
use crate::server::AppState;
use crate::server::state::stage_a_identity_is_destructive;
use djinn_coordinator::startup_census::{GoneProvenance, StartupCensus, TaskRunWitness};
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::{
    CreateTaskAttemptParams, Database, SessionRepository, TaskAttemptRepository, TaskRepository,
    TaskRunRepository,
};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};
use djinn_supervisor::ConnectionRegistry;

/// The configured-inventory fixture is consumed only by census acquisition.
/// Stage A receives the immutable result, so it has no opportunity to relist.
struct MatrixInventory {
    listed: Vec<WorkloadRecord>,
    presence: HashMap<String, ObjectPresence>,
}

/// Server-level inventory fixture. It records census acquisition calls so the
/// following Stage A/B/C handoff can prove it consumed immutable evidence.
struct CountingInventory {
    listed: Result<Vec<WorkloadRecord>, String>,
    presence: HashMap<String, ObjectPresence>,
    list_calls: AtomicUsize,
    presence_calls: AtomicUsize,
}

impl CountingInventory {
    fn listed(records: Vec<WorkloadRecord>, presence: HashMap<String, ObjectPresence>) -> Self {
        Self {
            listed: Ok(records),
            presence,
            list_calls: AtomicUsize::new(0),
            presence_calls: AtomicUsize::new(0),
        }
    }

    fn unavailable() -> Self {
        Self {
            listed: Err("apiserver unavailable".to_owned()),
            presence: HashMap::new(),
            list_calls: AtomicUsize::new(0),
            presence_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl WorkloadInventory for CountingInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        self.listed.clone()
    }

    async fn get_uid(&self, _: WorkloadObjectKind, _: &str, _: &str) -> UidGetResult {
        UidGetResult::Uncertain
    }

    async fn presence(&self, _: WorkloadObjectKind, name: &str) -> ObjectPresence {
        self.presence_calls.fetch_add(1, Ordering::SeqCst);
        self.presence
            .get(name)
            .cloned()
            .unwrap_or(ObjectPresence::Uncertain)
    }
}

/// The configured Stage A identity table is exact: only disconnected,
/// non-terminal starting/running rows with positive Gone evidence transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_stage_a_identity_matrix_uses_only_positive_gone_evidence() {
    let db = create_test_db();
    let events = test_events();
    let live = "matrix-live-running";
    let project_id = seed_running_session_with_task_run(&db, &events, live).await;
    let task_id = TaskRunRepository::new(db.clone())
        .get(live)
        .await
        .expect("read seed run")
        .expect("seed run exists")
        .task_id;
    let absent_starting = "matrix-absent-starting";
    let absent_running = "matrix-absent-running";
    let terminal_starting = "matrix-terminal-starting";
    let terminal_running = "matrix-terminal-running";
    let unknown = "matrix-unknown-running";
    let connected_gone = "matrix-connected-gone";
    let completed = "matrix-ledger-completed";
    let failed = "matrix-ledger-failed";
    let future = "matrix-ledger-future";
    let absent_starting_session = seed_session_for_run(
        &db,
        &events,
        &project_id,
        &task_id,
        absent_starting,
        "starting",
    )
    .await;
    let absent_running_session = seed_session_for_run(
        &db,
        &events,
        &project_id,
        &task_id,
        absent_running,
        "running",
    )
    .await;
    let terminal_starting_session = seed_session_for_run(
        &db,
        &events,
        &project_id,
        &task_id,
        terminal_starting,
        "starting",
    )
    .await;
    let terminal_running_session = seed_session_for_run(
        &db,
        &events,
        &project_id,
        &task_id,
        terminal_running,
        "running",
    )
    .await;
    let unknown_session =
        seed_session_for_run(&db, &events, &project_id, &task_id, unknown, "running").await;
    let connected_session = seed_session_for_run(
        &db,
        &events,
        &project_id,
        &task_id,
        connected_gone,
        "running",
    )
    .await;
    let completed_session =
        seed_session_for_run(&db, &events, &project_id, &task_id, completed, "completed").await;
    let failed_session =
        seed_session_for_run(&db, &events, &project_id, &task_id, failed, "failed").await;
    let future_session =
        seed_session_for_run(&db, &events, &project_id, &task_id, future, "future_status").await;
    let null_session = SessionRepository::new(db.clone(), events.clone())
        .create(CreateSessionParams {
            project_id: &project_id,
            task_id: Some(&task_id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create null-identity session")
        .id;

    let mut presence = HashMap::new();
    for run_id in [absent_starting, absent_running, connected_gone] {
        presence.insert(djinn_k8s::taskrun_job_name(run_id), ObjectPresence::Absent);
    }
    presence.insert(
        djinn_k8s::taskrun_job_name(unknown),
        ObjectPresence::Uncertain,
    );
    let census = StartupCensus::acquire(
        db.clone(),
        Some(Arc::new(MatrixInventory {
            listed: vec![
                job(live, false),
                job(terminal_starting, true),
                job(terminal_running, true),
            ],
            presence,
        })),
    )
    .await
    .expect("acquire configured startup census");
    assert!(
        census
            .runs()
            .iter()
            .any(|run| run.task_run_id == live && run.witness == TaskRunWitness::Live)
    );
    assert!(
        census
            .runs()
            .iter()
            .any(|run| run.task_run_id == unknown && run.witness == TaskRunWitness::Unknown)
    );
    assert_eq!(
        census
            .runs()
            .iter()
            .filter(|run| matches!(
                run.witness,
                TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)
            ))
            .count(),
        3
    );
    assert_eq!(
        census
            .runs()
            .iter()
            .filter(|run| matches!(
                run.witness,
                TaskRunWitness::Gone(GoneProvenance::TerminalPresent)
            ))
            .count(),
        2
    );
    let gone_ids = census
        .runs()
        .iter()
        .filter(|run| matches!(run.witness, TaskRunWitness::Gone(_)))
        .map(|run| run.task_run_id.as_str())
        .collect();
    assert!(!stage_a_identity_is_destructive(
        Some("   "),
        &gone_ids,
        false
    ));
    assert!(!stage_a_identity_is_destructive(
        Some("matrix-missing-ledger-row"),
        &gone_ids,
        false
    ));
    for invalid_ledger_identity in [completed, failed, future] {
        assert!(!stage_a_identity_is_destructive(
            Some(invalid_ledger_identity),
            &gone_ids,
            false
        ));
    }

    let state = AppState::new(db.clone(), tokio_util::sync::CancellationToken::new());
    state
        .rpc_registry()
        .register_connected_for_test(connected_gone)
        .await;
    state
        .interrupt_stale_sessions_on_startup_with_census(&census)
        .await;
    let repo = SessionRepository::new(db, events);
    for id in [
        absent_starting_session,
        absent_running_session,
        terminal_starting_session,
        terminal_running_session,
    ] {
        assert_eq!(
            repo.get(&id)
                .await
                .expect("read interrupted session")
                .expect("session exists")
                .status,
            "interrupted"
        );
    }
    for id in [
        connected_session,
        unknown_session,
        null_session,
        completed_session,
        failed_session,
        future_session,
    ] {
        assert_eq!(
            repo.get(&id)
                .await
                .expect("read preserved session")
                .expect("session exists")
                .status,
            "running"
        );
    }
    assert_eq!(
        repo.list_active()
            .await
            .expect("list remaining sessions")
            .len(),
        7,
        "exactly four of eleven matrix sessions transition; seven fail closed"
    );
}

struct UnavailableInventory;

#[async_trait::async_trait]
impl WorkloadInventory for UnavailableInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Err("unavailable".into())
    }
    async fn get_uid(&self, _: WorkloadObjectKind, _: &str, _: &str) -> UidGetResult {
        UidGetResult::Uncertain
    }
    async fn presence(&self, _: WorkloadObjectKind, _: &str) -> ObjectPresence {
        ObjectPresence::Uncertain
    }
}

async fn seed_startup_rows(db: &Database, events: &EventBus, run_id: &str) -> (String, String) {
    seed_startup_rows_with_status(db, events, run_id, "running").await
}

async fn seed_startup_rows_with_status(
    db: &Database,
    events: &EventBus,
    run_id: &str,
    status: &str,
) -> (String, String) {
    seed_session_with_task_run_status(db, events, run_id, status).await;
    let run = TaskRunRepository::new(db.clone())
        .get(run_id)
        .await
        .unwrap()
        .unwrap();
    let session = SessionRepository::new(db.clone(), events.clone())
        .list_active()
        .await
        .unwrap()[0]
        .id
        .clone();
    let attempt = format!("attempt-{run_id}");
    let dispatch_key = format!("dispatch-{run_id}");
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt,
            task_id: &run.task_id,
            role: "worker",
            dispatch_key: &dispatch_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();
    (session, attempt)
}

/// A complete server-owned startup lifecycle fixture.  Its `run` method is
/// deliberately the only place these regressions invoke the reapers: census
/// capture happens first, Stage A consumes that immutable value, and then the
/// coordinator's production Stage B/C handoff consumes the same value.
struct FullStartupFixture {
    db: Database,
    events: EventBus,
    run_id: String,
    session_id: String,
    attempt_id: String,
    inventory: Arc<CountingInventory>,
}

impl FullStartupFixture {
    async fn seeded(run_id: &str, inventory: CountingInventory) -> Self {
        Self::seeded_with_status(run_id, "running", inventory).await
    }

    async fn seeded_with_status(run_id: &str, status: &str, inventory: CountingInventory) -> Self {
        let db = create_test_db();
        let events = test_events();
        let (session_id, attempt_id) =
            seed_startup_rows_with_status(&db, &events, run_id, status).await;
        Self {
            db,
            events,
            run_id: run_id.to_owned(),
            session_id,
            attempt_id,
            inventory: Arc::new(inventory),
        }
    }

    async fn run(&self, age_attempt: bool) -> StartupCensus {
        let census = StartupCensus::acquire(self.db.clone(), Some(self.inventory.clone()))
            .await
            .expect("capture startup census before every lifecycle mutation");
        let state = AppState::new(self.db.clone(), tokio_util::sync::CancellationToken::new());
        state
            .interrupt_stale_sessions_on_startup_with_census(&census)
            .await;
        if age_attempt {
            tokio::time::sleep(std::time::Duration::from_secs(11)).await;
        }
        djinn_coordinator::complete_startup_reaper_phase(
            &self.db,
            "startup-census-fixture-incarnation",
            Some(&census),
        )
        .await;
        census
    }

    async fn durable_statuses(&self) -> (String, String, String) {
        let session = SessionRepository::new(self.db.clone(), self.events.clone())
            .get(&self.session_id)
            .await
            .expect("read fixture session")
            .expect("fixture session exists");
        let run = TaskRunRepository::new(self.db.clone())
            .get(&self.run_id)
            .await
            .expect("read fixture task run")
            .expect("fixture task run exists");
        let attempt = TaskAttemptRepository::new(self.db.clone())
            .get(&self.attempt_id)
            .await
            .expect("read fixture attempt")
            .expect("fixture attempt exists");
        (session.status, run.status, attempt.outcome)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reaper_preserves_live_pods() {
    let run_id = "startup-live-pod";
    let fixture = FullStartupFixture::seeded(
        run_id,
        CountingInventory::listed(vec![job(run_id, false)], HashMap::new()),
    )
    .await;

    let census = fixture.run(false).await;

    assert!(
        census
            .runs()
            .iter()
            .any(|run| run.task_run_id == run_id && run.witness == TaskRunWitness::Live)
    );
    assert_eq!(
        fixture.durable_statuses().await,
        ("running".into(), "running".into(), "pending".into())
    );
    assert_eq!(fixture.inventory.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.inventory.presence_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reaper_still_reaps_absent_running_job() {
    let run_id = "startup-absent-running";
    let mut presence = HashMap::new();
    presence.insert(djinn_k8s::taskrun_job_name(run_id), ObjectPresence::Absent);
    let fixture =
        FullStartupFixture::seeded(run_id, CountingInventory::listed(vec![], presence)).await;

    let census = fixture.run(true).await;

    assert!(census.runs().iter().any(|run| run.task_run_id == run_id
        && run.witness == TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)));
    assert_eq!(
        fixture.durable_statuses().await,
        (
            "interrupted".into(),
            "interrupted".into(),
            "interrupted".into()
        )
    );
    let attempt = TaskAttemptRepository::new(fixture.db.clone())
        .get(&fixture.attempt_id)
        .await
        .unwrap()
        .unwrap();
    let evidence: serde_json::Value = serde_json::from_str(
        attempt
            .summary_json
            .as_deref()
            .expect("startup reap records durable environmental evidence"),
    )
    .expect("startup environmental evidence is structured JSON");
    assert_eq!(evidence["failure_class"], "environmental_restart_orphan");
    assert_eq!(evidence["reason"], "startup");
    assert!(
        evidence["boot_incarnation_id"].is_string(),
        "restart-orphan evidence must identify the boot that proved absence"
    );
    assert_eq!(
        evidence["owner_classification"],
        "restart_orphan_null_owner"
    );
    assert_eq!(fixture.inventory.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.inventory.presence_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reaper_still_reaps_terminal_job() {
    for (run_id, status) in [
        ("startup-terminal-starting", "starting"),
        ("startup-terminal-running", "running"),
    ] {
        let fixture = FullStartupFixture::seeded_with_status(
            run_id,
            status,
            CountingInventory::listed(vec![job(run_id, true)], HashMap::new()),
        )
        .await;
        let census = fixture.run(true).await;

        assert!(census.runs().iter().any(|run| run.task_run_id == run_id
            && run.witness == TaskRunWitness::Gone(GoneProvenance::TerminalPresent)));
        assert_eq!(
            fixture.durable_statuses().await,
            (
                "interrupted".into(),
                "interrupted".into(),
                "interrupted".into()
            ),
            "terminal Job must reap the {status} fixture"
        );
        assert_eq!(fixture.inventory.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.inventory.presence_calls.load(Ordering::SeqCst), 0);
    }

    // The present, non-terminal starting control is otherwise identical to the
    // terminal-starting case and proves terminal provenance—not mere presence—
    // authorizes all three startup stages.
    let run_id = "startup-live-starting";
    let fixture = FullStartupFixture::seeded_with_status(
        run_id,
        "starting",
        CountingInventory::listed(vec![job(run_id, false)], HashMap::new()),
    )
    .await;
    let census = fixture.run(true).await;
    assert!(
        census
            .runs()
            .iter()
            .any(|run| run.task_run_id == run_id && run.witness == TaskRunWitness::Live)
    );
    assert_eq!(
        fixture.durable_statuses().await,
        ("running".into(), "starting".into(), "pending".into()),
        "a present non-terminal starting Job must produce 0/0/0 transitions"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[tracing_test::traced_test]
async fn startup_reaper_fails_closed_on_unknown() {
    for (run_id, inventory) in [
        ("startup-list-unavailable", CountingInventory::unavailable()),
        (
            "startup-get-uncertain",
            CountingInventory::listed(vec![], HashMap::new()),
        ),
    ] {
        let fixture = FullStartupFixture::seeded(run_id, inventory).await;
        let census = fixture.run(false).await;
        assert!(
            census
                .runs()
                .iter()
                .any(|run| run.task_run_id == run_id && run.witness == TaskRunWitness::Unknown)
        );
        assert_eq!(
            fixture.durable_statuses().await,
            ("running".into(), "running".into(), "pending".into())
        );
        assert_eq!(fixture.inventory.list_calls.load(Ordering::SeqCst), 1);
    }
    for stage in ["startup_stage_a", "startup_stage_b", "startup_stage_c"] {
        assert!(
            logs_contain(&format!("stage=\"{stage}\"")) && logs_contain("reason=\"unknown\""),
            "{stage} must emit a structured reason=unknown startup deferral"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reaper_not_configured_is_legacy() {
    let events = test_events();
    let legacy_db = create_test_db();
    let (legacy_session, legacy_attempt) =
        seed_startup_rows(&legacy_db, &events, "legacy-startup-run").await;
    let legacy = StartupCensus::acquire(legacy_db.clone(), None)
        .await
        .unwrap();
    let unavailable_db = create_test_db();
    let (unavailable_session, unavailable_attempt) =
        seed_startup_rows(&unavailable_db, &events, "unavailable-startup-run").await;
    let unavailable =
        StartupCensus::acquire(unavailable_db.clone(), Some(Arc::new(UnavailableInventory)))
            .await
            .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(11)).await;

    let state = AppState::new(
        legacy_db.clone(),
        tokio_util::sync::CancellationToken::new(),
    );
    state
        .interrupt_stale_sessions_on_startup_with_census(&legacy)
        .await;
    djinn_coordinator::complete_startup_reaper_phase(&legacy_db, "new-incarnation", Some(&legacy))
        .await;
    assert_eq!(
        SessionRepository::new(legacy_db.clone(), events.clone())
            .get(&legacy_session)
            .await
            .unwrap()
            .unwrap()
            .status,
        "interrupted"
    );
    assert_eq!(
        TaskRunRepository::new(legacy_db.clone())
            .get("legacy-startup-run")
            .await
            .unwrap()
            .unwrap()
            .status,
        "interrupted"
    );
    assert_eq!(
        TaskAttemptRepository::new(legacy_db)
            .get(&legacy_attempt)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "interrupted"
    );

    let state = AppState::new(
        unavailable_db.clone(),
        tokio_util::sync::CancellationToken::new(),
    );
    state
        .interrupt_stale_sessions_on_startup_with_census(&unavailable)
        .await;
    djinn_coordinator::complete_startup_reaper_phase(
        &unavailable_db,
        "new-incarnation",
        Some(&unavailable),
    )
    .await;
    assert_eq!(
        SessionRepository::new(unavailable_db.clone(), events)
            .get(&unavailable_session)
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert_eq!(
        TaskRunRepository::new(unavailable_db.clone())
            .get("unavailable-startup-run")
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert_eq!(
        TaskAttemptRepository::new(unavailable_db)
            .get(&unavailable_attempt)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        "pending"
    );
}

#[async_trait::async_trait]
impl WorkloadInventory for MatrixInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        Ok(self.listed.clone())
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

fn job(run_id: &str, terminal: bool) -> WorkloadRecord {
    WorkloadRecord {
        kind: WorkloadObjectKind::Job,
        name: djinn_k8s::taskrun_job_name(run_id),
        uid: None,
        labels: BTreeMap::new(),
        terminal,
        images: Vec::new(),
        commands: Vec::new(),
    }
}

fn create_test_db() -> Database {
    Database::open_in_memory().expect("open in-memory test database")
}

fn test_events() -> EventBus {
    EventBus::noop()
}

/// Seed a project, epic, task, task-run, and a running session with the
/// given `task_run_id`.  Returns the project id.
async fn seed_running_session_with_task_run(
    db: &Database,
    events: &EventBus,
    task_run_id: &str,
) -> String {
    seed_session_with_task_run_status(db, events, task_run_id, "running").await
}

async fn seed_session_with_task_run_status(
    db: &Database,
    events: &EventBus,
    task_run_id: &str,
    status: &str,
) -> String {
    use djinn_db::{EpicCreateInput, EpicRepository, ProjectRepository};

    let project_repo = ProjectRepository::new(db.clone(), events.clone());
    let project = project_repo
        .create("reconnectability-test", "owner", "repo")
        .await
        .expect("create test project");

    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let epic = epic_repo
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "test-epic",
                description: "",
                emoji: "",
                color: "",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("create test epic");

    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let task = task_repo
        .create(&epic.id, "test-task", "", "", "task", 0, "", Some("open"))
        .await
        .expect("create test task");

    // Seed the task-run row (FK constraint on sessions.task_run_id).
    TaskRunRepository::new(db.clone())
        .create(djinn_db::CreateTaskRunParams {
            id: task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some(status),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create test task run");

    // Seed a session with status = 'running' and the given task_run_id.
    let session_repo = SessionRepository::new(db.clone(), events.clone());
    session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create test session");

    project.id
}

async fn seed_session_for_run(
    db: &Database,
    events: &EventBus,
    project_id: &str,
    task_id: &str,
    task_run_id: &str,
    status: &str,
) -> String {
    TaskRunRepository::new(db.clone())
        .create(djinn_db::CreateTaskRunParams {
            id: task_run_id,
            project_id,
            task_id,
            trigger_type: "manual",
            status: Some(status),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create matrix task run");
    SessionRepository::new(db.clone(), events.clone())
        .create(CreateSessionParams {
            project_id,
            task_id: Some(task_id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create matrix session")
        .id
}

/// A deterministic test that creates one running session with a `task_run_id`
/// and registers that id as connected in `ConnectionRegistry`, then asserts
/// the startup reconnectability measurement reports
/// `running_sessions == 1` and `connected_or_reconnectable_sessions == 1`.
///
/// This fixture proves that the old blanket `interrupt_all_running` path
/// would have interrupted a connected worker — triggering the gated
/// follow-up implementation in proposal `phif`.
///
/// The measurement is observed *before* the startup interruption mutation,
/// using the extracted `measure_startup_reconnectability` helper so the
/// test asserts the pre-mutation result directly (AC 7/8).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connected_worker_has_numerator_one_before_startup_interruption() {
    let db = create_test_db();
    let events = test_events();
    let task_run_id = "test-run-connected-1";

    // Seed one running session with a task_run_id.
    seed_running_session_with_task_run(&db, &events, task_run_id).await;

    // Register the task_run_id as connected in the ConnectionRegistry so
    // `is_connected(task_run_id)` returns true without a real TCP worker
    // handshake.
    let registry = Arc::new(ConnectionRegistry::new());
    registry.register_connected_for_test(task_run_id).await;

    // Build an AppState wired to our test DB and registry.
    // AppState::new allocates its own ConnectionRegistry eagerly; register
    // the task_run_id on THAT registry via the public accessor.
    let cancel = tokio_util::sync::CancellationToken::new();
    let state = AppState::new(db.clone(), cancel);
    state
        .rpc_registry()
        .register_connected_for_test(task_run_id)
        .await;

    let session_repo = SessionRepository::new(db.clone(), events.clone());

    // Exercise the measurement BEFORE any mutation.
    let measurement = state.measure_startup_reconnectability(&session_repo).await;

    // ── Assertions ──────────────────────────────────────────────────────────
    assert_eq!(
        measurement.running_sessions, 1,
        "there must be exactly one running session in the measurement denominator"
    );
    assert_eq!(
        measurement.connected_or_reconnectable_sessions, 1,
        "the connected worker must be counted in the numerator (proposal phif nonzero-measurement decision rule)"
    );
    assert_eq!(
        measurement.grace_window_ms, 10_000,
        "the startup grace window must be 10,000ms per the proposal spec"
    );
    assert!(
        !measurement.startup_instance_id.is_empty(),
        "startup_instance_id must be a non-empty UUID v7"
    );
    assert!(
        measurement
            .reconnectable_task_run_ids()
            .contains(task_run_id),
        "the exact connected task_run_id must be exposed for startup mutation"
    );

    // Verify the session was NOT mutated by the measurement — it is still running.
    let running_after = session_repo
        .list_active()
        .await
        .expect("list_active after measurement");
    assert_eq!(
        running_after.len(),
        1,
        "measurement must not mutate session status; session should still be running"
    );
    assert_eq!(
        running_after[0].task_run_id.as_deref(),
        Some(task_run_id),
        "the running session must retain its task_run_id"
    );
}

/// When the startup reconnectability measurement reports a connected worker,
/// `interrupt_stale_sessions_on_startup` must preserve that running session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_preserves_connected_running_session() {
    let db = create_test_db();
    let events = test_events();
    let task_run_id = "test-run-preserve-1";

    seed_running_session_with_task_run(&db, &events, task_run_id).await;

    let cancel = tokio_util::sync::CancellationToken::new();
    let state = AppState::new(db.clone(), cancel);
    state
        .rpc_registry()
        .register_connected_for_test(task_run_id)
        .await;

    // Drive the full startup interruption path. The method is private and
    // only reachable from the integration-style test via crate-internal
    // visibility; this test file lives under the crate's `tests` module.
    state.interrupt_stale_sessions_on_startup().await;

    let running_after = SessionRepository::new(db.clone(), events.clone())
        .list_active()
        .await
        .expect("list_active after startup interruption");
    assert_eq!(
        running_after.len(),
        1,
        "the connected running session must be preserved"
    );
    assert_eq!(
        running_after[0].task_run_id.as_deref(),
        Some(task_run_id),
        "preserved session must retain its task_run_id"
    );
}

/// When no running sessions are reconnectable, the startup interruption path
/// must behave like the old blanket `interrupt_all_running()` and interrupt
/// every running session, including rows with a NULL task_run_id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_reconnectable_startup_interrupts_all_running_sessions() {
    let db = create_test_db();
    let events = test_events();
    let task_run_id = "test-run-stale-1";

    let project_id = seed_running_session_with_task_run(&db, &events, task_run_id).await;

    // Add a second running session with no task_run_id to prove NULL
    // identities are not preserved when the reconnectability set is empty.
    // Go through the repository layer so test setup obeys the server raw-SQL
    // boundary just like production code.
    let session_repo = SessionRepository::new(db.clone(), events.clone());
    session_repo
        .create(CreateSessionParams {
            project_id: &project_id,
            task_id: None,
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create NULL task_run_id session");

    let running_before = session_repo
        .list_active()
        .await
        .expect("list_active before startup interruption");
    assert_eq!(
        running_before.len(),
        2,
        "fixture must have two running sessions"
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    let state = AppState::new(db.clone(), cancel);
    // Do NOT register any connected workers; reconnectability is zero.

    // Drive the full startup interruption path. The 10-second grace probe is
    // included but finds no connected workers, so the blanket path runs.
    state.interrupt_stale_sessions_on_startup().await;

    let running_after = session_repo
        .list_active()
        .await
        .expect("list_active after startup interruption");
    assert!(
        running_after.is_empty(),
        "all running sessions must be interrupted when reconnectability is zero"
    );

    // Verify every previously-running session is now interrupted.
    for session in running_before {
        let after = SessionRepository::new(db.clone(), events.clone())
            .get(&session.id)
            .await
            .expect("fetch session after interruption")
            .expect("session must exist");
        assert_eq!(
            after.status,
            djinn_core::models::SessionStatus::Interrupted.as_str(),
            "session {} must be interrupted",
            session.id
        );
    }
}

/// Verify reconnectability is derived from unique task-run identities rather
/// than incrementing once per running session row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_running_sessions_for_one_task_run_count_once() {
    let db = create_test_db();
    let events = test_events();
    let task_run_id = "test-run-connected-duplicate";

    let project_id = seed_running_session_with_task_run(&db, &events, task_run_id).await;
    let session_repo = SessionRepository::new(db.clone(), events.clone());
    session_repo
        .create(CreateSessionParams {
            project_id: &project_id,
            task_id: None,
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(task_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create duplicate running session for task run");

    let cancel = tokio_util::sync::CancellationToken::new();
    let state = AppState::new(db.clone(), cancel);
    state
        .rpc_registry()
        .register_connected_for_test(task_run_id)
        .await;

    let measurement = state.measure_startup_reconnectability(&session_repo).await;

    assert_eq!(measurement.running_sessions, 2);
    assert_eq!(
        measurement.connected_or_reconnectable_sessions, 1,
        "one connected task_run_id must only contribute one reconnectable identity"
    );
    assert_eq!(measurement.reconnectable_task_run_ids().len(), 1);
    assert!(
        measurement
            .reconnectable_task_run_ids()
            .contains(task_run_id)
    );
}

/// Verify the measurement returns zero counts when no running sessions exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_startup_reports_zero_reconnectability() {
    let db = create_test_db();
    let events = test_events();
    let cancel = tokio_util::sync::CancellationToken::new();
    let state = AppState::new(db, cancel);
    let session_repo = SessionRepository::new(state.db().clone(), events.clone());

    let measurement = state.measure_startup_reconnectability(&session_repo).await;

    assert_eq!(measurement.running_sessions, 0);
    assert_eq!(measurement.connected_or_reconnectable_sessions, 0);
    assert_eq!(measurement.grace_window_ms, 10_000);
}

/// Verify the measurement counts a running session with a task_run_id that
/// is NOT registered as connected — numerator should be 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_worker_has_numerator_zero() {
    let db = create_test_db();
    let events = test_events();
    let task_run_id = "test-run-disconnected-1";

    seed_running_session_with_task_run(&db, &events, task_run_id).await;

    let cancel = tokio_util::sync::CancellationToken::new();
    let state = AppState::new(db.clone(), cancel);
    // Do NOT register the task_run_id in the registry — it is disconnected.
    let session_repo = SessionRepository::new(db.clone(), events.clone());

    // The grace probe fires but finds nothing for a disconnected session.
    // This exercises the production code path as-is (10s sleep).
    let measurement = state.measure_startup_reconnectability(&session_repo).await;

    assert_eq!(measurement.running_sessions, 1);
    assert_eq!(
        measurement.connected_or_reconnectable_sessions, 0,
        "a disconnected session must not be counted as reconnectable"
    );
}
