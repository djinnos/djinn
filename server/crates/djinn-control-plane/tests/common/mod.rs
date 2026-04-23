//! Shared fixtures for control-plane integration tests.
//!
//! Integration tests in a crate's `tests/` directory compile as separate crates
//! and do not share modules automatically. Each test file that needs these
//! helpers should `#[path = "common/mod.rs"] mod common;` at the top.
//!
//! Mirrors the helpers previously in `server/src/test_helpers.rs` but trimmed
//! to only the bits needed by the migrated contract tests: DB-side seeding of
//! projects / epics / tasks / sessions, kept small enough that every
//! integration test file can pull it in without pulling in Axum.

#![allow(dead_code)]

use djinn_core::events::EventBus;
use djinn_core::models::{Epic, Project, SessionRecord, Task};
use djinn_core::paths::project_dir;
use djinn_db::{
    Database, EpicCreateInput, EpicRepository, NoteRepository, ProjectRepository,
    SessionRepository, TaskRepository, repositories::session::CreateSessionParams,
};
use djinn_memory::Note;

pub fn test_events() -> EventBus {
    EventBus::noop()
}

pub fn workspace_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).expect("create control-plane test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create control-plane test tempdir")
}

pub async fn create_test_project(db: &Database) -> Project {
    let repo = ProjectRepository::new(db.clone(), test_events());
    let id = uuid::Uuid::now_v7();
    let name = format!("test-project-{id}");
    // Tests don't depend on a specific owner/repo; use a deterministic
    // pair derived from the generated name so slug-based lookups still work.
    repo.create(&name, "test", &name)
        .await
        .expect("failed to create test project")
}

/// Create a project backed by a real temporary directory on disk. Returns the
/// project and a `TempDir` guard — the directory is cleaned up when the guard
/// drops.
///
/// The temporary directory is still created so tests that want a real
/// filesystem location can use it, but the DB row no longer records the
/// path (project dirs are derived from `{DJINN_HOME}/projects/{owner}/{repo}`
/// at runtime).
pub async fn create_test_project_with_dir(db: &Database) -> (Project, tempfile::TempDir) {
    let dir = workspace_tempdir("cp-test-project-");
    let repo = ProjectRepository::new(db.clone(), test_events());
    let name = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let project = repo
        .create(&name, "test", &name)
        .await
        .expect("failed to create test project");
    (project, dir)
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
            status: None,
            auto_breakdown: None,
            originating_adr_id: None,
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
            None,
        )
        .await
        .expect("failed to create test task");
    assert_eq!(task.status, "open");
    // Ensure tasks have AC so Start transitions succeed in tests.
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

pub async fn create_test_session(
    db: &Database,
    project_id: &str,
    task_id: &str,
) -> SessionRecord {
    let repo = SessionRepository::new(db.clone(), test_events());
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

pub async fn create_test_note(db: &Database, project_id: &str) -> Note {
    let repo = NoteRepository::new(db.clone(), test_events());
    let project_repo = ProjectRepository::new(db.clone(), test_events());
    let project = project_repo
        .get(project_id)
        .await
        .expect("failed to load project for note")
        .expect("project not found for note");

    let project_path = project_dir(&project.github_owner, &project.github_repo);
    std::fs::create_dir_all(&project_path).expect("failed to create test project path");

    repo.create(
        project_id,
        &project_path,
        "test note",
        "test note body",
        "research",
        "[]",
    )
    .await
    .expect("failed to create test note")
}
