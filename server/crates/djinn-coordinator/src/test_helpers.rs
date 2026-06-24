//! Test helpers for djinn-coordinator tests.
//!
//! Mirrors `djinn-agent::test_helpers` for coordinator test fixtures.

use std::sync::Arc;

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{Database, EpicRepository, ProjectRepository, TaskRepository};

pub fn test_events() -> EventBus {
    let (tx, _rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(64);
    super::events::event_bus_for(&tx)
}

pub fn create_test_db() -> Database {
    Database::new_in_memory()
}

pub async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    let events = test_events();
    let repo = ProjectRepository::new(db.clone(), events);
    repo.create(
        "test-owner",
        "test-repo",
        Some("https://github.com/test-owner/test-repo"),
    )
    .await
    .expect("create project")
}

pub async fn create_test_epic(db: &Database, project_id: &str) -> djinn_core::models::Epic {
    let events = test_events();
    let repo = EpicRepository::new(db.clone(), events);
    repo.create(project_id, "Test Epic", "A test epic", "[]", "proposal-1")
        .await
        .expect("create epic")
}

pub async fn create_test_task(
    db: &Database,
    project_id: &str,
    epic_id: &str,
) -> djinn_core::models::Task {
    let events = test_events();
    let repo = TaskRepository::new(db.clone(), events);
    repo.create(
        project_id,
        Some(epic_id),
        "Test Task",
        "A test task",
        "",
        "task",
        1,
        "[]",
        "[]",
    )
    .await
    .expect("create task")
}

pub fn agent_context_from_db(
    db: Database,
    cancel: tokio_util::sync::CancellationToken,
) -> super::context::AgentContext {
    let events = test_events();
    super::context::AgentContext {
        db: db.clone(),
        event_bus: events,
        git_actors: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        background_work_tasks: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        role_registry: Arc::new(super::roles_compat::RoleRegistry::new()),
        health_tracker: djinn_provider::catalog::health::HealthTracker::new(),
        file_time: Arc::new(super::file_time::FileTime::new()),
        lsp: djinn_lsp::LspManager::new(),
        catalog: djinn_provider::catalog::CatalogService::new(db.clone()),
        coordinator: Arc::new(tokio::sync::Mutex::new(None)),
        active_tasks: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        task_ops_project_path_override: None,
        working_root: None,
        graph_warmer: None,
        repo_graph_ops: None,
        runtime_ops: None,
        cargo_target_runs_root: None,
        mirror: None,
        rpc_registry: None,
        default_project_id: None,
        reconciliation_sweep: super::context::ReconciliationSweepConfig::default(),
    }
}
