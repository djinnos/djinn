//! Test utilities for djinn-agent tests.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use djinn_core::events::EventBus;
use djinn_core::models::Project;
use djinn_core::models::{Epic, Task};
use djinn_db::{Database, EpicCreateInput, EpicRepository, ProjectRepository, TaskRepository};

use crate::context::AgentContext;
use crate::file_time::FileTime;
use crate::lsp::LspManager;
use crate::roles::RoleRegistry;
use djinn_provider::catalog::{CatalogService, HealthTracker};

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

pub fn test_events() -> EventBus {
    EventBus::noop()
}

pub fn agent_context_from_db(db: Database, _cancel: CancellationToken) -> AgentContext {
    AgentContext {
        db,
        event_bus: EventBus::noop(),
        git_actors: Arc::new(Mutex::new(HashMap::new())),
        verifying_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
        role_registry: Arc::new(RoleRegistry::new()),
        health_tracker: HealthTracker::new(),
        file_time: Arc::new(FileTime::new()),
        lsp: LspManager::new(),
        catalog: CatalogService::new(),
        coordinator: Arc::new(tokio::sync::Mutex::new(None)),
        active_tasks: crate::context::ActivityTracker::default(),
    }
}

pub async fn create_test_project(db: &Database) -> Project {
    let repo = ProjectRepository::new(db.clone(), test_events());
    let id = uuid::Uuid::now_v7();
    let path = format!("/tmp/djinn-test-project-{id}");
    let name = format!("test-project-{id}");
    repo.create(&name, &path)
        .await
        .expect("failed to create test project")
}

pub async fn create_test_epic(db: &Database, project_id: &str) -> Epic {
    let repo = EpicRepository::new(db.clone(), test_events());
    repo.create_for_project(
        project_id,
        EpicCreateInput {
            title: "test-epic",
            description: "test epic description",
            emoji: "🧪",
            color: "blue",
            owner: "test-owner",
            memory_refs: None,
        },
    )
    .await
    .expect("failed to create test epic")
}

pub async fn create_test_task(db: &Database, project_id: &str, epic_id: &str) -> Task {
    let repo = TaskRepository::new(db.clone(), test_events());
    let task = repo
        .create_in_project(
            project_id,
            Some(epic_id),
            "test-task",
            "test task description",
            "test task design",
            "task",
            2,
            "test-owner",
            None,
        )
        .await
        .expect("failed to create test task");
    repo.update(
        &task.id,
        &task.title,
        &task.description,
        &task.design,
        task.priority,
        &task.owner,
        &task.labels,
        r#"[{"description":"default test criterion","met":false}]"#,
    )
    .await
    .expect("failed to set test task acceptance criteria")
}
