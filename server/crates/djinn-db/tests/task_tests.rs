//! Integration tests for the task repository and task state machine.
//!
//! These tests exercise `djinn-db` and `djinn-core` domain logic only —
//! no `AppState` or server-only types. They require a live Dolt test
//! instance (see `djinn_db::Database::open_in_memory`).

pub(crate) use djinn_core::events::EventBus;
pub(crate) use djinn_core::models::{Task, TaskStatus, TransitionAction};
pub(crate) use djinn_db::Database;
pub(crate) use djinn_db::EpicRepository;
pub(crate) use djinn_db::Error;
pub(crate) use djinn_db::test_support::event_bus_for;
pub(crate) use djinn_db::{ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository};
pub(crate) use rstest::rstest;
pub(crate) use tokio::sync::broadcast;

#[path = "task_tests/closed_task_sync.rs"]
mod closed_task_sync;
#[path = "task_tests/existing.rs"]
mod existing;
#[path = "task_tests/filter_matrices.rs"]
mod filter_matrices;
#[path = "task_tests/state_machine.rs"]
mod state_machine;
#[path = "task_tests/sync_terminal_state.rs"]
mod sync_terminal_state;

// ── Local test helpers (pure djinn-db / djinn-core) ──────────────────────────

/// Create an in-memory database with all migrations applied.
pub(crate) fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Construct an `EventBus` that drops every event.
pub(crate) fn noop_events() -> EventBus {
    EventBus::noop()
}

pub(crate) async fn make_epic(db: &Database, events: EventBus) -> djinn_core::models::Epic {
    EpicRepository::new(db.clone(), events)
        .create("Test Epic", "", "", "", "", None)
        .await
        .unwrap()
}

pub(crate) async fn open_task(repo: &TaskRepository, epic_id: &str) -> Task {
    let task = repo
        .create(epic_id, "T", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    repo.update(
        &task.id,
        "T",
        "",
        "",
        0,
        "",
        "",
        r#"[{"description":"default","met":false}]"#,
    )
    .await
    .unwrap()
}

pub(crate) fn make_peer_task(
    id: &str,
    project_id: &str,
    epic_id: &str,
    status: &str,
    updated_at: &str,
) -> Task {
    Task {
        id: id.to_string(),
        project_id: project_id.to_string(),
        short_id: format!("p{}", &id[..3]),
        epic_id: Some(epic_id.to_string()),
        title: "Peer Task".to_string(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: status.to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        verification_failure_count: 0,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: updated_at.to_string(),
        closed_at: if status == "closed" {
            Some(updated_at.to_string())
        } else {
            None
        },
        close_reason: if status == "closed" {
            Some("completed".to_string())
        } else {
            None
        },
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        total_verification_failure_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    }
}

// Helpers replacing the server-only `test_helpers::create_test_{project,epic,session}`.
// They only use djinn-db / djinn-core APIs.

pub(crate) async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    use djinn_db::ProjectRepository;
    let repo = ProjectRepository::new(db.clone(), noop_events());
    let id = uuid::Uuid::now_v7();
    let path = format!("/tmp/djinn-test-project-{id}");
    let name = format!("test-project-{id}");
    repo.create(&name, &path)
        .await
        .expect("failed to create test project")
}

pub(crate) async fn create_test_epic(
    db: &Database,
    project_id: &str,
) -> djinn_core::models::Epic {
    use djinn_db::{EpicCreateInput, EpicRepository};
    let repo = EpicRepository::new(db.clone(), noop_events());
    repo.create_for_project(
        project_id,
        EpicCreateInput {
            title: "test-epic",
            description: "test epic description",
            emoji: "🧪",
            color: "blue",
            owner: "test-owner",
            memory_refs: None,
            status: None,
            auto_breakdown: None,
            originating_adr_id: None,
        },
    )
    .await
    .expect("failed to create test epic")
}

pub(crate) async fn create_test_session(
    db: &Database,
    project_id: &str,
    task_id: &str,
) -> djinn_core::models::SessionRecord {
    use djinn_db::SessionRepository;
    use djinn_db::repositories::session::CreateSessionParams;
    let repo = SessionRepository::new(db.clone(), noop_events());
    repo.create(CreateSessionParams {
        project_id,
        task_id: Some(task_id),
        model: "test-model",
        agent_type: "worker",
        metadata_json: None,
        task_run_id: None,
    })
    .await
    .expect("failed to create test session")
}
