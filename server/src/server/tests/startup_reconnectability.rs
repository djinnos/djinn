//! Deterministic test coverage for the startup reconnectability measurement
//! (proposal `phif` AC 7/8).
//!
//! This module verifies that the measurement seam in
//! [`AppState::measure_startup_reconnectability`] correctly counts running
//! sessions and their connected-worker status **before** the startup
//! blanket-interruption mutation runs.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::events::EventBus;
use crate::server::AppState;
use djinn_coordinator::startup_census::{
    GoneProvenance, InventoryAvailability, StartupCensus, TaskRunWitness,
};
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::test_support::{
    backdate_task_attempt_created_at, backdate_task_run_started_at, capture_queries,
    seed_legacy_session_without_task_run_ledger_for_test,
};
use djinn_db::{
    CreateTaskAttemptParams, Database, SessionRepository, TaskAttemptRepository, TaskRepository,
    TaskRunRepository,
};
use djinn_k8s::{
    ObjectPresence, UidGetResult, WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};
use djinn_supervisor::ConnectionRegistry;
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct TraceBuffer(Arc<Mutex<Vec<u8>>>);

struct TraceBufferWriter(TraceBuffer);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TraceBuffer {
    type Writer = TraceBufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TraceBufferWriter(self.clone())
    }
}

impl Write for TraceBufferWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.0.lock().expect("trace buffer lock").extend(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl TraceBuffer {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().expect("trace buffer lock").clone())
            .expect("structured tracing is UTF-8")
    }
}

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
async fn startup_stage_a_identity_matrix() {
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
    let disconnected_live = "matrix-disconnected-live";
    let completed = "matrix-ledger-completed";
    let failed = "matrix-ledger-failed";
    let future = "matrix-ledger-future";
    let blank = "   ";
    let missing = "matrix-missing-ledger-row";
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
    let disconnected_live_session = seed_session_for_run(
        &db,
        &events,
        &project_id,
        &task_id,
        disconnected_live,
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
    // These identities deliberately have no task-run ledger row. They still
    // must traverse the production Stage A session enumeration and fail
    // closed, rather than being tested only through the identity predicate.
    let blank_session =
        seed_session_without_ledger(&db, &events, &project_id, &task_id, blank).await;
    let missing_session =
        seed_session_without_ledger(&db, &events, &project_id, &task_id, missing).await;

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
                // `connected=false` is not absence evidence: this disconnected
                // worker remains present in the immutable cluster census.
                job(disconnected_live, false),
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
    // Persist linked attempts before the production Stage-A entry point.  This
    // asserts that identity handling is not merely an enum-level decision.
    for (index, run_id) in [
        absent_starting,
        absent_running,
        terminal_starting,
        terminal_running,
        unknown,
        connected_gone,
        disconnected_live,
        completed,
        failed,
        future,
        blank,
        missing,
    ]
    .into_iter()
    .enumerate()
    {
        seed_attempt_for_task(&db, &task_id, &format!("matrix-attempt-{index}"), run_id).await;
    }

    let state = AppState::new(db.clone(), tokio_util::sync::CancellationToken::new());
    state
        .rpc_registry()
        .register_connected_for_test(connected_gone)
        .await;
    state
        .interrupt_stale_sessions_on_startup_with_census(&census)
        .await;
    let repo = SessionRepository::new(db.clone(), events);
    assert_eq!(
        repo.list_for_task_run(live)
            .await
            .expect("list live matrix session")
            .as_slice()
            .iter()
            .map(|session| session.status.as_str())
            .collect::<Vec<_>>(),
        ["running"],
        "the Live census witness preserves its durable linked session"
    );
    for id in [
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
        // A durable `starting` row whose Job is authoritatively absent may
        // simply not have been CREATEd yet, so Stage A carries the census's
        // durable run state and fences it exactly as Stage B and Stage C do.
        absent_starting_session,
        // The registry intentionally has no connection for this identity; the
        // Live census witness—not connection absence—preserves it.
        disconnected_live_session,
        unknown_session,
        null_session,
        blank_session,
        missing_session,
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
        11,
        "exactly three of fourteen matrix sessions transition; eleven fail closed"
    );
    for (run_id, expected_status) in [
        (live, "running"),
        (absent_starting, "starting"),
        (absent_running, "running"),
        (terminal_starting, "starting"),
        (terminal_running, "running"),
        (unknown, "running"),
        (connected_gone, "running"),
        (disconnected_live, "running"),
        (completed, "completed"),
        (failed, "failed"),
        (future, "future_status"),
    ] {
        assert_eq!(
            TaskRunRepository::new(db.clone())
                .get(run_id)
                .await
                .expect("read durable matrix task run")
                .expect("durable matrix task run exists")
                .status,
            expected_status,
            "Stage A must not mutate task-run {run_id}"
        );
    }
    assert_eq!(
        TaskRunRepository::new(db.clone())
            .list_for_task(&task_id)
            .await
            .expect("list durable matrix task runs")
            .len(),
        11,
        "Stage A must preserve all eleven durable matrix ledger rows"
    );
    assert_eq!(
        TaskAttemptRepository::new(db)
            .list_pending_before("9999-01-01T00:00:00.000Z")
            .await
            .expect("list durable matrix attempts")
            .len(),
        12,
        "Stage A must not terminalize linked attempts"
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
    // Durable attempt IDs are VARCHAR(36). Run IDs in these descriptive
    // startup fixtures may be longer, so use the UUID-shaped identity that
    // production uses instead of deriving an overlong primary key from one.
    let attempt = uuid::Uuid::now_v7().to_string();
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

    async fn add_task_run(&self, run_id: &str, status: &str) -> String {
        let primary = TaskRunRepository::new(self.db.clone())
            .get(&self.run_id)
            .await
            .expect("read primary fixture run")
            .expect("primary fixture run exists");
        let session_id = seed_session_for_run(
            &self.db,
            &self.events,
            &primary.project_id,
            &primary.task_id,
            run_id,
            status,
        )
        .await;
        if status == "starting" {
            TaskRunRepository::new(self.db.clone())
                .update_status(run_id, djinn_core::models::TaskRunStatus::Starting)
                .await
                .expect("restore secondary starting state");
        }
        session_id
    }

    async fn session_status(&self, session_id: &str) -> String {
        SessionRepository::new(self.db.clone(), self.events.clone())
            .get(session_id)
            .await
            .expect("read fixture session")
            .expect("fixture session exists")
            .status
    }

    async fn task_run_status(&self, run_id: &str) -> String {
        TaskRunRepository::new(self.db.clone())
            .get(run_id)
            .await
            .expect("read fixture task run")
            .expect("fixture task run exists")
            .status
    }
}

/// SQLx emits these statements from its execution path, so this is a real
/// repository/query spy rather than a counter maintained by reaper code.
fn assert_stage_c_does_not_requery_liveness(trace: &djinn_db::test_support::QueryTrace) {
    assert!(trace.round_trips() > 0, "SQL observer must not be vacuous");
    let statements = trace
        .statements
        .iter()
        .map(|statement| statement.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let stage_b_end = statements
        .iter()
        .rposition(|statement| statement.contains("update task_runs"))
        .expect("Stage B must persist its authorized mutation");
    assert!(
        statements[stage_b_end + 1..]
            .iter()
            .all(|statement| !statement.contains("from sessions")
                && !statement.contains("from task_runs")),
        "Stage C queried post-Stage-B liveness instead of the census:\n{}",
        trace.rendered()
    );
}

/// Exercise the production startup ordering with linked durable records. The
/// post-Stage-B run is interrupted before Stage C handles the attempt, proving
/// that C consumes the immutable census rather than rebuilding liveness from
/// the task-run state changed by B.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_census_precedes_every_mutation() {
    let run_id = "startup-immutable-census-order";
    let mut presence = HashMap::new();
    presence.insert(djinn_k8s::taskrun_job_name(run_id), ObjectPresence::Absent);
    let fixture =
        FullStartupFixture::seeded(run_id, CountingInventory::listed(Vec::new(), presence)).await;

    let census = StartupCensus::acquire(fixture.db.clone(), Some(fixture.inventory.clone()))
        .await
        .expect("capture the immutable census before Stage A");
    assert_eq!(fixture.inventory.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.inventory.presence_calls.load(Ordering::SeqCst), 1);
    assert!(census.runs().iter().any(|run| {
        run.task_run_id == run_id
            && run.witness == TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)
    }));
    assert_eq!(
        fixture.durable_statuses().await,
        ("running".into(), "running".into(), "pending".into()),
        "census acquisition must complete before every lifecycle mutation"
    );

    let state = AppState::new(
        fixture.db.clone(),
        tokio_util::sync::CancellationToken::new(),
    );
    state
        .interrupt_stale_sessions_on_startup_with_census(&census)
        .await;
    assert_eq!(
        fixture.durable_statuses().await,
        ("interrupted".into(), "running".into(), "pending".into()),
        "Stage A consumes the same census before the coordinator mutates task runs"
    );

    tokio::time::sleep(std::time::Duration::from_secs(11)).await;
    let ((), query_trace) = capture_queries(djinn_coordinator::complete_startup_reaper_phase(
        &fixture.db,
        "startup-immutable-census-incarnation",
        Some(&census),
    ))
    .await;
    assert_stage_c_does_not_requery_liveness(&query_trace);
    assert_eq!(
        fixture.durable_statuses().await,
        (
            "interrupted".into(),
            "interrupted".into(),
            "interrupted".into()
        ),
        "Stage C classifies the linked attempt from pre-Stage-B census evidence"
    );
    assert_eq!(fixture.inventory.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.inventory.presence_calls.load(Ordering::SeqCst), 1);
    assert!(census.runs().iter().any(|run| {
        run.task_run_id == run_id
            && run.witness == TaskRunWitness::Gone(GoneProvenance::AuthoritativelyAbsent)
    }));
}

/// Projection coverage through the full server sequence. A second historical
/// durable run for the same task makes Stage C consume the task projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_stage_c_task_projection() {
    for (index, (name, second_status, listed_second, first_presence, second_presence, expected)) in
        [
            (
                "gone-live",
                "running",
                true,
                ObjectPresence::Absent,
                ObjectPresence::Uncertain,
                "pending",
            ),
            (
                "gone-creation-transit",
                "starting",
                false,
                ObjectPresence::Absent,
                ObjectPresence::Absent,
                "pending",
            ),
            (
                "gone-unknown",
                "running",
                false,
                ObjectPresence::Absent,
                ObjectPresence::Uncertain,
                "pending",
            ),
            (
                "all-gone",
                "running",
                false,
                ObjectPresence::Absent,
                ObjectPresence::Absent,
                "interrupted",
            ),
        ]
        .into_iter()
        .enumerate()
    {
        // Task-run IDs are VARCHAR(36); keep the descriptive case name in
        // assertions while using compact, stable durable fixture identities.
        let primary = format!("projection-{index}-primary");
        let secondary = format!("projection-{index}-secondary");
        let mut presence = HashMap::from([(djinn_k8s::taskrun_job_name(&primary), first_presence)]);
        presence.insert(djinn_k8s::taskrun_job_name(&secondary), second_presence);
        let listed = listed_second
            .then(|| job(&secondary, false))
            .into_iter()
            .collect();
        let fixture =
            FullStartupFixture::seeded(&primary, CountingInventory::listed(listed, presence)).await;
        let secondary_session = fixture.add_task_run(&secondary, second_status).await;
        backdate_task_attempt_created_at(&fixture.db, &fixture.attempt_id, "1 minute").await;
        let census = StartupCensus::acquire(fixture.db.clone(), Some(fixture.inventory.clone()))
            .await
            .expect("acquire one production census");
        let task_id = TaskRunRepository::new(fixture.db.clone())
            .get(&primary)
            .await
            .expect("read projection task")
            .expect("projection task exists")
            .task_id;
        assert!(
            census.task_projection(&task_id).is_some(),
            "non-terminal durable runs must not project NotApplicable"
        );
        AppState::new(
            fixture.db.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
        .interrupt_stale_sessions_on_startup_with_census(&census)
        .await;
        let secondary_stage_a_session = match name {
            // `gone-creation-transit` is a durable `starting` row with
            // authoritative absence: Stage A carries that durable run state and
            // preserves the linked session, matching Stage B's fence and the
            // task-level `CreationTransit` projection Stage C consumes.
            "gone-live" | "gone-unknown" | "gone-creation-transit" => "running",
            "all-gone" => "interrupted",
            _ => unreachable!("projection matrix row is exhaustive"),
        };
        assert_eq!(
            fixture.session_status(&fixture.session_id).await,
            "interrupted",
            "Stage A must consume Gone evidence for the primary linked session"
        );
        assert_eq!(
            fixture.session_status(&secondary_session).await,
            secondary_stage_a_session,
            "Stage A linked secondary session for {name}"
        );
        assert_eq!(
            fixture.task_run_status(&primary).await,
            "running",
            "Stage A does not mutate the primary task-run ledger"
        );
        assert_eq!(
            fixture.task_run_status(&secondary).await,
            second_status,
            "Stage A does not mutate the historical/non-terminal task-run ledger"
        );
        assert_eq!(
            TaskAttemptRepository::new(fixture.db.clone())
                .get(&fixture.attempt_id)
                .await
                .expect("read Stage-A attempt")
                .expect("Stage-A attempt exists")
                .outcome,
            "pending",
            "Stage A does not classify pending attempts"
        );
        let traces = TraceBuffer::default();
        let (_, trace) = if name == "gone-unknown" {
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(traces.clone())
                .finish();
            async {
                tracing::callsite::rebuild_interest_cache();
                capture_queries(djinn_coordinator::complete_startup_reaper_phase(
                    &fixture.db,
                    "projection-census-incarnation",
                    Some(&census),
                ))
                .await
            }
            .with_subscriber(subscriber)
            .await
        } else {
            // A scoped tracing subscriber replaces the SQLx query observer.
            // Only Unknown needs trace capture; leave the observer installed
            // for the destructive row's no-post-Stage-B-query proof.
            capture_queries(djinn_coordinator::complete_startup_reaper_phase(
                &fixture.db,
                "projection-census-incarnation",
                Some(&census),
            ))
            .await
        };
        if name == "all-gone" {
            assert_stage_c_does_not_requery_liveness(&trace);
        }
        if name == "gone-unknown" {
            assert!(
                traces.contents().lines().any(|line| {
                    line.contains("stage=\"startup_stage_c\"")
                        && line.contains("reason=\"unknown\"")
                }),
                "Gone+Unknown must emit the Stage C structured unknown deferral"
            );
        }
        assert_eq!(
            fixture.session_status(&fixture.session_id).await,
            "interrupted"
        );
        assert_eq!(fixture.task_run_status(&primary).await, "interrupted");
        let (secondary_session_status, secondary_run_status) = match name {
            "gone-live" | "gone-unknown" => ("running", "running"),
            // The starting row's commit-then-CREATE window is fenced in all
            // three stages, so neither its session nor its ledger row moves.
            "gone-creation-transit" => ("running", "starting"),
            "all-gone" => ("interrupted", "interrupted"),
            _ => unreachable!("projection matrix row is exhaustive"),
        };
        assert_eq!(
            fixture.session_status(&secondary_session).await,
            secondary_session_status,
            "linked secondary session for {name}"
        );
        assert_eq!(
            fixture.task_run_status(&secondary).await,
            secondary_run_status,
            "historical/non-terminal task run for {name}"
        );
        assert_eq!(
            TaskAttemptRepository::new(fixture.db.clone())
                .get(&fixture.attempt_id)
                .await
                .expect("read projected attempt")
                .expect("projected attempt exists")
                .outcome,
            expected,
            "projection case {name}"
        );
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
    // Exercise the actual retry policy against the exact row Stage C just
    // terminalized, rather than reconstructing similar JSON in a unit test.
    let (actor, cancel) = djinn_coordinator::test_helpers::make_coordinator_actor_cancellable(
        &fixture.db,
        &tokio::sync::broadcast::channel(8).0,
    );
    let task_id = TaskRunRepository::new(fixture.db.clone())
        .get(run_id)
        .await
        .expect("read reaped task run")
        .expect("reaped task run exists")
        .task_id;
    let decision = actor
        .latest_attempt_strike_decision(&task_id, "worker")
        .await
        .expect("reaper-produced attempt has a strike decision");
    assert!(
        decision.exempted,
        "startup restart orphan must be strike-exempt"
    );
    assert_eq!(
        decision.decision,
        djinn_telemetry::dispatch::STRIKE_DECISION_EXEMPTED
    );
    assert_eq!(
        decision.source,
        djinn_telemetry::dispatch::STRIKE_SOURCE_ENVIRONMENTAL_RESTART_ORPHAN
    );
    cancel.cancel();
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
async fn startup_reaper_fails_closed_on_unknown() {
    let traces = TraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(traces.clone())
        .finish();

    async {
        // A dependency callsite may have been cached as disabled by another
        // concurrently-running test's crate-scoped subscriber. Re-evaluate it
        // while this future's all-crate collector is the active dispatch.
        tracing::callsite::rebuild_interest_cache();
        for (run_id, inventory) in [
            ("startup-list-unavailable", CountingInventory::unavailable()),
            (
                "startup-get-uncertain",
                CountingInventory::listed(vec![], HashMap::new()),
            ),
        ] {
            let fixture = FullStartupFixture::seeded(run_id, inventory).await;
            // Stage C intentionally considers only aged pending attempts. Age this
            // linked row so the full sequence reaches its fail-closed projection.
            let census = fixture.run(true).await;
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
    }
    .with_subscriber(subscriber)
    .await;

    let logs = traces.contents();
    for stage in ["startup_stage_a", "startup_stage_b", "startup_stage_c"] {
        assert!(
            logs.lines().any(|line| {
                line.contains(&format!("stage=\"{stage}\"")) && line.contains("reason=\"unknown\"")
            }),
            "{stage} must emit a structured reason=unknown startup deferral"
        );
    }
}

/// Pin the exact pre-change Stage A/B/C transition table for
/// `workload_inventory: None`.
///
/// Legacy startup recovery is deliberately identity-blind: Stage A interrupts
/// every running session except those whose worker is *currently connected*,
/// Stage B interrupts every non-terminal task run past a wall-clock age
/// threshold — including the connected worker's run — and Stage C classifies
/// every aged pending attempt whose task then has neither a live run nor a
/// running session. Configuration absence must keep producing exactly that
/// table, and must never silently become the configured-inventory
/// preserve-all/reap-all behaviour, so each stage boundary is snapshotted
/// separately rather than only the phase's final outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reaper_not_configured_is_legacy() {
    let staged = StartupTable::seed("legacy-staged").await;
    let seeded_snapshot = vec![
        ("running", "running", "running", "pending"),
        ("starting", "running", "starting", "pending"),
        ("connected", "running", "running", "pending"),
        ("terminal-ledger", "running", "completed", "pending"),
        ("null-identity", "running", "absent", "pending"),
        ("blank-identity", "running", "absent", "pending"),
    ];
    assert_eq!(staged.snapshot().await, owned(&seeded_snapshot));
    assert_eq!(staged.aggregates().await, (6, 3, 6));

    let legacy = StartupCensus::acquire(staged.db.clone(), None)
        .await
        .expect("acquire legacy startup census");
    assert_eq!(legacy.availability(), InventoryAvailability::NotConfigured);

    // ── Stage A ──────────────────────────────────────────────────────────
    staged
        .state
        .interrupt_stale_sessions_on_startup_with_census(&legacy)
        .await;
    let after_stage_a = vec![
        ("running", "interrupted", "running", "pending"),
        ("starting", "interrupted", "starting", "pending"),
        // Live RPC connectivity is the only preservation lever legacy Stage A
        // has: not durable state, not cluster evidence.
        ("connected", "running", "running", "pending"),
        ("terminal-ledger", "interrupted", "completed", "pending"),
        ("null-identity", "interrupted", "absent", "pending"),
        ("blank-identity", "interrupted", "absent", "pending"),
    ];
    assert_eq!(
        staged.snapshot().await,
        owned(&after_stage_a),
        "legacy Stage A must interrupt every running session except the connected worker"
    );
    assert_eq!(staged.aggregates().await, (1, 3, 6));

    // ── Stage B ──────────────────────────────────────────────────────────
    djinn_coordinator::test_helpers::run_startup_reaper_stage_b(&staged.db, Some(&legacy)).await;
    let after_stage_b = vec![
        ("running", "interrupted", "interrupted", "pending"),
        // Legacy Stage B reaps `starting` as readily as `running`: the
        // durable-commit/CREATE window it cannot see is exactly the hazard the
        // configured census fences.
        ("starting", "interrupted", "interrupted", "pending"),
        // ...and it reaps the connected worker's run that Stage A preserved.
        ("connected", "running", "interrupted", "pending"),
        ("terminal-ledger", "interrupted", "completed", "pending"),
        ("null-identity", "interrupted", "absent", "pending"),
        ("blank-identity", "interrupted", "absent", "pending"),
    ];
    assert_eq!(
        staged.snapshot().await,
        owned(&after_stage_b),
        "legacy Stage B must interrupt every aged non-terminal run regardless of identity"
    );
    assert_eq!(staged.aggregates().await, (1, 0, 6));

    // ── Stage C ──────────────────────────────────────────────────────────
    djinn_coordinator::test_helpers::run_startup_reaper_stage_c(
        &staged.db,
        "legacy-startup-incarnation",
        Some(&legacy),
    )
    .await;
    let after_stage_c = vec![
        ("running", "interrupted", "interrupted", "interrupted"),
        ("starting", "interrupted", "interrupted", "interrupted"),
        // The still-running session is what withholds this attempt from the
        // legacy orphan query — post-mutation database state, not census
        // evidence.
        ("connected", "running", "interrupted", "pending"),
        ("terminal-ledger", "interrupted", "completed", "interrupted"),
        ("null-identity", "interrupted", "absent", "interrupted"),
        ("blank-identity", "interrupted", "absent", "interrupted"),
    ];
    assert_eq!(
        staged.snapshot().await,
        owned(&after_stage_c),
        "legacy Stage C must classify every aged orphan whose task lost both liveness signals"
    );
    assert_eq!(staged.aggregates().await, (1, 0, 1));

    // The staged run drives the same two halves the production phase calls.
    // Replaying the identical table through `complete_startup_reaper_phase`
    // keeps that composition load-bearing: dropping either stage from the
    // production entry point diverges here.
    let composed = StartupTable::seed("legacy-composed").await;
    let composed_census = StartupCensus::acquire(composed.db.clone(), None)
        .await
        .expect("acquire composed legacy startup census");
    composed
        .state
        .interrupt_stale_sessions_on_startup_with_census(&composed_census)
        .await;
    djinn_coordinator::complete_startup_reaper_phase(
        &composed.db,
        "legacy-startup-incarnation",
        Some(&composed_census),
    )
    .await;
    assert_eq!(
        composed.snapshot().await,
        owned(&after_stage_c),
        "complete_startup_reaper_phase must compose exactly Stage B then Stage C"
    );
    assert_eq!(composed.aggregates().await, (1, 0, 1));

    // ── Configured-but-Unavailable control ───────────────────────────────
    // Same durable table, same stage boundaries, configured inventory whose
    // LIST failed. Unknown evidence fails closed at every stage; legacy
    // NotConfigured is never collapsed into it.
    let unavailable_table = StartupTable::seed("unavailable-staged").await;
    let unavailable = StartupCensus::acquire(
        unavailable_table.db.clone(),
        Some(Arc::new(UnavailableInventory)),
    )
    .await
    .expect("acquire unavailable startup census");
    assert_eq!(
        unavailable.availability(),
        InventoryAvailability::Unavailable
    );
    assert!(
        unavailable
            .runs()
            .iter()
            .all(|run| run.witness == TaskRunWitness::Unknown),
        "an unavailable LIST yields no positive Gone provenance"
    );

    unavailable_table
        .state
        .interrupt_stale_sessions_on_startup_with_census(&unavailable)
        .await;
    assert_eq!(
        unavailable_table.snapshot().await,
        owned(&seeded_snapshot),
        "configured-unavailable Stage A authorizes no session transition"
    );
    assert_eq!(unavailable_table.aggregates().await, (6, 3, 6));

    djinn_coordinator::test_helpers::run_startup_reaper_stage_b(
        &unavailable_table.db,
        Some(&unavailable),
    )
    .await;
    assert_eq!(
        unavailable_table.snapshot().await,
        owned(&seeded_snapshot),
        "configured-unavailable Stage B authorizes no task-run transition"
    );
    assert_eq!(unavailable_table.aggregates().await, (6, 3, 6));

    djinn_coordinator::test_helpers::run_startup_reaper_stage_c(
        &unavailable_table.db,
        "unavailable-startup-incarnation",
        Some(&unavailable),
    )
    .await;
    assert_eq!(
        unavailable_table.snapshot().await,
        owned(&seeded_snapshot),
        "configured-unavailable Stage C authorizes no attempt classification"
    );
    assert_eq!(unavailable_table.aggregates().await, (6, 3, 6));
}

/// Widen a borrowed expected transition table into the owned shape
/// [`StartupTable::snapshot`] returns.
fn owned(rows: &[(&'static str, &str, &str, &str)]) -> Vec<(&'static str, String, String, String)> {
    rows.iter()
        .map(|(label, session, run, attempt)| {
            (
                *label,
                (*session).to_owned(),
                (*run).to_owned(),
                (*attempt).to_owned(),
            )
        })
        .collect()
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
        ready: false,
        deployment_revision: None,
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

    // Session creation represents a connected worker and promotes a linked
    // dispatch row to `running`. Starting-state startup fixtures model the
    // earlier durable-commit/CREATE window, so restore that requested state.
    if status == "starting" {
        TaskRunRepository::new(db.clone())
            .update_status(task_run_id, djinn_core::models::TaskRunStatus::Starting)
            .await
            .expect("restore requested starting task-run fixture state");
    }

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
    let session_id = SessionRepository::new(db.clone(), events.clone())
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
        .id;

    // `SessionRepository::create` promotes a linked starting run. Restore the
    // committed pre-CREATE state so the matrix contains real starting rows.
    if status == "starting" {
        TaskRunRepository::new(db.clone())
            .update_status(task_run_id, djinn_core::models::TaskRunStatus::Starting)
            .await
            .expect("restore matrix starting task-run state");
    }

    session_id
}

async fn seed_session_without_ledger(
    db: &Database,
    events: &EventBus,
    project_id: &str,
    task_id: &str,
    task_run_id: &str,
) -> String {
    // Production data normally cannot outlive the task-run FK. The startup
    // census must nevertheless fail closed for durable historical rows from
    // before that invariant, so construct that persisted legacy shape only in
    // this isolated test database.
    seed_legacy_session_without_task_run_ledger_for_test(
        db,
        events.clone(),
        CreateSessionParams {
            project_id,
            task_id: Some(task_id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(task_run_id),
            pricing: None,
            cost_basis: None,
        },
    )
    .await
    .id
}

async fn seed_attempt_for_task(db: &Database, task_id: &str, attempt_id: &str, key: &str) {
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: attempt_id,
            task_id,
            role: "worker",
            dispatch_key: &format!("matrix-dispatch-{key}"),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create durable matrix attempt");
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

/// One durable row of the startup transition table: an isolated task with its
/// own optional task-run ledger row, one `running` session, and one `pending`
/// attempt. Each row owns its own task so Stage C's per-task orphan query
/// resolves every row independently instead of collapsing into one aggregate.
struct StartupTableRow {
    label: &'static str,
    run_id: Option<String>,
    session_id: String,
    attempt_id: String,
}

/// How a table row's durable session identity is constructed.
enum StartupRowIdentity {
    /// A real ledger row in the given durable status.
    Ledger(&'static str),
    /// `sessions.task_run_id IS NULL`.
    Null,
    /// A whitespace-only identity with no ledger row (pre-FK historical shape).
    Blank,
}

/// The six-row startup transition table plus the durable database holding it.
struct StartupTable {
    db: Database,
    events: EventBus,
    state: AppState,
    rows: Vec<StartupTableRow>,
}

impl StartupTable {
    /// Seed the identical row set into a fresh database. `connected_label`
    /// names the single row registered as a connected worker, which is the
    /// only lever the pre-change Stage A had.
    async fn seed(prefix: &str) -> Self {
        let db = create_test_db();
        let events = test_events();
        let mut rows = Vec::new();
        for (label, identity) in [
            ("running", StartupRowIdentity::Ledger("running")),
            ("starting", StartupRowIdentity::Ledger("starting")),
            ("connected", StartupRowIdentity::Ledger("running")),
            ("terminal-ledger", StartupRowIdentity::Ledger("completed")),
            ("null-identity", StartupRowIdentity::Null),
            ("blank-identity", StartupRowIdentity::Blank),
        ] {
            rows.push(seed_startup_table_row(&db, &events, prefix, label, identity).await);
        }
        let state = AppState::new(db.clone(), tokio_util::sync::CancellationToken::new());
        state
            .rpc_registry()
            .register_connected_for_test(&format!("{prefix}-connected"))
            .await;
        Self {
            db,
            events,
            state,
            rows,
        }
    }

    /// `(label, session status, task-run status or `absent`, attempt outcome)`
    /// for every seeded row, in table order.
    async fn snapshot(&self) -> Vec<(&'static str, String, String, String)> {
        let sessions = SessionRepository::new(self.db.clone(), self.events.clone());
        let runs = TaskRunRepository::new(self.db.clone());
        let attempts = TaskAttemptRepository::new(self.db.clone());
        let mut out = Vec::new();
        for row in &self.rows {
            let session = sessions
                .get(&row.session_id)
                .await
                .expect("read startup table session")
                .expect("startup table session exists")
                .status;
            let run = match row.run_id.as_deref() {
                Some(run_id) => {
                    runs.get(run_id)
                        .await
                        .expect("read startup table task run")
                        .expect("startup table task run exists")
                        .status
                }
                None => "absent".to_owned(),
            };
            let attempt = attempts
                .get(&row.attempt_id)
                .await
                .expect("read startup table attempt")
                .expect("startup table attempt exists")
                .outcome;
            out.push((row.label, session, run, attempt));
        }
        out
    }

    /// Aggregate counts `(running sessions, live task runs, pending attempts)`.
    async fn aggregates(&self) -> (usize, usize, usize) {
        let sessions = SessionRepository::new(self.db.clone(), self.events.clone())
            .list_active()
            .await
            .expect("list startup table sessions")
            .len();
        let live_runs = TaskRunRepository::new(self.db.clone())
            .list_startup_live()
            .await
            .expect("list startup table live runs")
            .len();
        let pending = TaskAttemptRepository::new(self.db.clone())
            .list_pending_before("9999-01-01T00:00:00.000Z")
            .await
            .expect("list startup table pending attempts")
            .len();
        (sessions, live_runs, pending)
    }
}

async fn seed_startup_table_row(
    db: &Database,
    events: &EventBus,
    prefix: &str,
    label: &'static str,
    identity: StartupRowIdentity,
) -> StartupTableRow {
    use djinn_db::{EpicCreateInput, EpicRepository, ProjectRepository};

    let project = ProjectRepository::new(db.clone(), events.clone())
        .create(
            &format!("{prefix}-{label}"),
            "owner",
            &format!("{prefix}-{label}"),
        )
        .await
        .expect("create startup table project");
    let epic = EpicRepository::new(db.clone(), events.clone())
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "startup-table-epic",
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
        .expect("create startup table epic");
    let task = TaskRepository::new(db.clone(), events.clone())
        .create(&epic.id, label, "", "", "task", 0, "", Some("open"))
        .await
        .expect("create startup table task");

    let sessions = SessionRepository::new(db.clone(), events.clone());
    let (run_id, session_id) = match identity {
        StartupRowIdentity::Ledger(status) => {
            let run_id = format!("{prefix}-{label}");
            TaskRunRepository::new(db.clone())
                .create(djinn_db::CreateTaskRunParams {
                    id: &run_id,
                    project_id: &project.id,
                    task_id: &task.id,
                    trigger_type: "manual",
                    status: Some(status),
                    workspace_path: None,
                    mirror_ref: None,
                    dispatch_group_id: None,
                })
                .await
                .expect("create startup table task run");
            let session_id = sessions
                .create(startup_table_session_params(
                    &project.id,
                    &task.id,
                    Some(&run_id),
                ))
                .await
                .expect("create startup table session")
                .id;
            // `SessionRepository::create` promotes a linked starting run, so
            // restore the requested committed durable state.
            if status == "starting" {
                TaskRunRepository::new(db.clone())
                    .update_status(&run_id, djinn_core::models::TaskRunStatus::Starting)
                    .await
                    .expect("restore startup table durable starting run");
            }
            // The legacy Stage B threshold compares `started_at`; backdating
            // pins the table without sleeping past a wall-clock window.
            djinn_db::test_support::backdate_task_run_started_at(db, &run_id, "1 hour").await;
            (Some(run_id), session_id)
        }
        StartupRowIdentity::Null => {
            let session_id = sessions
                .create(startup_table_session_params(&project.id, &task.id, None))
                .await
                .expect("create null-identity startup table session")
                .id;
            (None, session_id)
        }
        StartupRowIdentity::Blank => {
            let session_id = seed_legacy_session_without_task_run_ledger_for_test(
                db,
                events.clone(),
                startup_table_session_params(&project.id, &task.id, Some("   ")),
            )
            .await
            .id;
            (None, session_id)
        }
    };

    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &format!("{prefix}-{label}-dispatch"),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create startup table attempt");
    backdate_task_attempt_created_at(db, &attempt_id, "1 hour").await;

    StartupTableRow {
        label,
        run_id,
        session_id,
        attempt_id,
    }
}

fn startup_table_session_params<'a>(
    project_id: &'a str,
    task_id: &'a str,
    task_run_id: Option<&'a str>,
) -> CreateSessionParams<'a> {
    CreateSessionParams {
        project_id,
        task_id: Some(task_id),
        model: "openai/gpt-5.5",
        agent_type: "worker",
        metadata_json: None,
        task_run_id,
        pricing: None,
        cost_basis: None,
    }
}

// ─── Production startup wiring (task 2j87) ───────────────────────────────────

/// Cluster inventory injected into the real `become_leader` startup path.
///
/// It answers only about the two fixture identities: the live run's Job is in
/// the LIST snapshot as non-terminal, the gone run's Job is authoritatively
/// absent. Everything else stays `Uncertain`, so nothing is destroyed on
/// unproven evidence.
struct LeaderStartupInventory {
    listed: Vec<WorkloadRecord>,
    presence: HashMap<String, ObjectPresence>,
}

#[async_trait::async_trait]
impl WorkloadInventory for LeaderStartupInventory {
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

/// One durable identity chain — task, task-run, session, pending attempt —
/// seeded old enough that the legacy startup transition table would act on it.
struct LeaderStartupRows {
    run_id: String,
    session_id: String,
    attempt_id: String,
}

async fn seed_leader_startup_project(db: &Database, events: &EventBus) -> (String, String) {
    use djinn_db::{EpicCreateInput, EpicRepository, ProjectRepository};

    let project = ProjectRepository::new(db.clone(), events.clone())
        .create("leader-wiring-test", "owner", "repo")
        .await
        .expect("create leader wiring project");
    let epic = EpicRepository::new(db.clone(), events.clone())
        .create_for_project(
            &project.id,
            EpicCreateInput {
                title: "leader-wiring-epic",
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
        .expect("create leader wiring epic");
    (project.id, epic.id)
}

async fn seed_leader_startup_rows(
    db: &Database,
    events: &EventBus,
    project_id: &str,
    epic_id: &str,
    run_id: &str,
) -> LeaderStartupRows {
    // A distinct task per identity: Stage C classifies attempts through the
    // census's per-TASK projection, so sharing one task would collapse the
    // live and gone identities into a single reduction.
    let task = TaskRepository::new(db.clone(), events.clone())
        .create(epic_id, run_id, "", "", "task", 0, "", Some("open"))
        .await
        .expect("create leader wiring task");
    TaskRunRepository::new(db.clone())
        .create(djinn_db::CreateTaskRunParams {
            id: run_id,
            project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create leader wiring task run");
    let session_repo = SessionRepository::new(db.clone(), events.clone());
    session_repo
        .create(startup_table_session_params(
            project_id,
            &task.id,
            Some(run_id),
        ))
        .await
        .expect("create leader wiring session");
    let session_id = session_repo
        .list_for_task_run(run_id)
        .await
        .expect("read seeded session")
        .first()
        .expect("seeded session exists")
        .id
        .clone();
    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &format!("dispatch-{run_id}"),
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .expect("seed pending attempt");
    // Both startup age gates are 10s. Backdating by a minute puts every row
    // past them while staying far inside the periodic sweeps' windows (4h for
    // task-runs, 5m for pending attempts), so the periodic reapers cannot be
    // the cause of any transition this regression observes.
    backdate_task_run_started_at(db, run_id, "60 seconds").await;
    backdate_task_attempt_created_at(db, &attempt_id, "60 seconds").await;
    LeaderStartupRows {
        run_id: run_id.to_owned(),
        session_id,
        attempt_id,
    }
}

async fn leader_startup_statuses(
    db: &Database,
    events: &EventBus,
    rows: &LeaderStartupRows,
) -> (String, String, String) {
    let session = SessionRepository::new(db.clone(), events.clone())
        .get(&rows.session_id)
        .await
        .expect("read fixture session")
        .expect("fixture session exists");
    let run = TaskRunRepository::new(db.clone())
        .get(&rows.run_id)
        .await
        .expect("read fixture task run")
        .expect("fixture task run exists");
    let attempt = TaskAttemptRepository::new(db.clone())
        .get(&rows.attempt_id)
        .await
        .expect("read fixture attempt")
        .expect("fixture attempt exists");
    (session.status, run.status, attempt.outcome)
}

fn leader_startup_intact() -> (String, String, String) {
    (
        "running".to_owned(),
        "running".to_owned(),
        "pending".to_owned(),
    )
}

/// The production startup wiring itself, driven through `AppState::become_leader`.
///
/// Every other regression in this epic composes `StartupCensus::acquire` ->
/// Stage A -> `complete_startup_reaper_phase` by hand, so all of them stay
/// green when the two lines that make the *server* do this are deleted. This
/// one calls no stage directly: it seeds durable rows, hands `become_leader` a
/// cluster inventory, and asserts the durable session/task-run/task-attempt
/// transitions that only occur when the census `become_leader` captures
/// reaches Stage A **and** is moved into coordinator startup for Stages B/C.
///
/// The two fixture identities are chosen so each production line owns a
/// distinct observable:
///
/// * deleting the Stage A call leaves the census-gone run's session `running`;
/// * deleting the `CoordinatorDeps::with_startup_census` handoff drops the
///   coordinator onto the legacy age-threshold table, which reaps the LIVE
///   run — the exact "a server restart is not evidence of death" regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn become_leader_drives_the_startup_census_through_every_stage() {
    let db = create_test_db();
    let events = test_events();
    let live_run = "leader-wiring-live-run";
    let gone_run = "leader-wiring-gone-run";
    let (project_id, epic_id) = seed_leader_startup_project(&db, &events).await;
    let live = seed_leader_startup_rows(&db, &events, &project_id, &epic_id, live_run).await;
    let gone = seed_leader_startup_rows(&db, &events, &project_id, &epic_id, gone_run).await;

    let mut presence = HashMap::new();
    presence.insert(
        djinn_k8s::taskrun_job_name(gone_run),
        ObjectPresence::Absent,
    );
    let inventory: Arc<dyn WorkloadInventory> = Arc::new(LeaderStartupInventory {
        listed: vec![job(live_run, false)],
        presence,
    });

    // Pre-mutation truth: nothing has moved yet.
    assert_eq!(
        leader_startup_statuses(&db, &events, &live).await,
        leader_startup_intact()
    );
    assert_eq!(
        leader_startup_statuses(&db, &events, &gone).await,
        leader_startup_intact()
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    let state = AppState::new(db.clone(), cancel.clone());
    state
        .set_test_startup_workload_inventory(inventory.clone())
        .await;

    // The only call this regression makes. No stage is invoked by hand.
    state.become_leader().await;

    // `become_leader` moves the census into coordinator startup, which
    // completes Stages B and C inside its own boot phase, so wait for the
    // durable effect rather than for a log line.
    let settled = tokio::time::timeout(std::time::Duration::from_secs(45), async {
        loop {
            let (_, run_status, attempt_outcome) =
                leader_startup_statuses(&db, &events, &gone).await;
            if run_status == "interrupted" && attempt_outcome == "interrupted" {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "coordinator startup never completed Stage B/C for the census-gone identity; \
         last observed (session, run, attempt) = {:?}",
        leader_startup_statuses(&db, &events, &gone).await
    );

    assert_eq!(
        leader_startup_statuses(&db, &events, &gone).await,
        (
            "interrupted".to_owned(),
            "interrupted".to_owned(),
            "interrupted".to_owned()
        ),
        "become_leader must drive its own census through Stage A (session), Stage B \
         (task run) and Stage C (attempt) for a run whose Job is authoritatively absent"
    );
    assert_eq!(
        leader_startup_statuses(&db, &events, &live).await,
        leader_startup_intact(),
        "a restart is not evidence of death: the run whose Job the census observed \
         alive keeps its session, its task run and its pending attempt, even though \
         every one of those rows is old enough for the legacy startup thresholds"
    );

    cancel.cancel();
}
