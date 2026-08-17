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
use crate::server::state::stage_a_identity_is_destructive;
use djinn_coordinator::startup_census::{GoneProvenance, StartupCensus, TaskRunWitness};
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::test_support::{backdate_task_attempt_created_at, capture_queries};
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
        // The registry intentionally has no connection for this identity; the
        // Live census witness—not connection absence—preserves it.
        disconnected_live_session,
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
        8,
        "exactly four of twelve matrix sessions transition; eight fail closed"
    );
    assert_eq!(
        TaskRunRepository::new(db.clone())
            .list_for_task(&task_id)
            .await
            .expect("list durable matrix task runs")
            .len(),
        10,
        "Stage A must preserve all linked ledger rows"
    );
    assert_eq!(
        TaskAttemptRepository::new(db)
            .list_pending_before("9999-01-01T00:00:00.000Z")
            .await
            .expect("list durable matrix attempts")
            .len(),
        10,
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
    for (name, second_status, listed_second, first_presence, second_presence, expected) in [
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
    ] {
        let primary = format!("projection-{name}-primary");
        let secondary = format!("projection-{name}-secondary");
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
            "gone-live" | "gone-unknown" => "running",
            "gone-creation-transit" | "all-gone" => "interrupted",
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
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(traces.clone())
            .finish();
        let ((), trace) = async {
            tracing::callsite::rebuild_interest_cache();
            capture_queries(djinn_coordinator::complete_startup_reaper_phase(
                &fixture.db,
                "projection-census-incarnation",
                Some(&census),
            ))
            .await
        }
        .with_subscriber(subscriber)
        .await;
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
            "gone-creation-transit" => ("interrupted", "starting"),
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
