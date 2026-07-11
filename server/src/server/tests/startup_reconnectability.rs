//! Deterministic test coverage for the startup reconnectability measurement
//! (proposal `phif` AC 7/8).
//!
//! This module verifies that the measurement seam in
//! [`AppState::measure_startup_reconnectability`] correctly counts running
//! sessions and their connected-worker status **before** the startup
//! blanket-interruption mutation runs.

use std::sync::Arc;

use crate::events::EventBus;
use crate::server::AppState;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::{Database, SessionRepository, TaskRepository, TaskRunRepository};
use djinn_supervisor::ConnectionRegistry;

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
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
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
