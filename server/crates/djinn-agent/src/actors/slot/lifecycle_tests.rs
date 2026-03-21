//! Integration-style lifecycle tests for `run_task_lifecycle`.
//!
//! Uses the provider-injection seam added to `TaskLifecycleParams` so no real
//! LLM calls are made.  Each test spins up a real (tmpdir) git repository
//! because `prepare_worktree` needs one; the git ops are fast and keep total
//! test time well under 500ms.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tempfile::{Builder, TempDir};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AgentType;
use crate::actors::slot::SlotEvent;
use crate::actors::slot::lifecycle::{TaskLifecycleParams, run_task_lifecycle};
use crate::roles::role_impl_for;
use crate::test_helpers::{
    FailingProvider, FakeProvider, agent_context_from_db, create_test_db, create_test_epic,
};
use djinn_core::models::SessionStatus;
use djinn_db::{ProjectRepository, SessionRepository, TaskRepository};

// ─── Git repo helpers ─────────────────────────────────────────────────────────

/// Creates a temporary git repository with a single commit on `main`.
async fn create_git_repo() -> TempDir {
    let tmp = Builder::new()
        .prefix("djinn-lifecycle-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let p = tmp.path();

    let run = |args: &[&str]| {
        let p = p.to_path_buf();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        async move {
            Command::new("git")
                .args(&args)
                .current_dir(&p)
                .output()
                .await
                .expect("git command")
        }
    };

    run(&["init"]).await;
    run(&["config", "user.email", "test@djinn.test"]).await;
    run(&["config", "user.name", "Test"]).await;
    tokio::fs::write(p.join("README.md"), "# test")
        .await
        .unwrap();
    run(&["add", "README.md"]).await;
    run(&["commit", "-m", "init"]).await;
    run(&["branch", "-M", "main"]).await;

    tmp
}

/// Registers a project in the DB pointing to `repo_path`.
async fn register_project(
    db: &djinn_db::Database,
    repo_path: &Path,
) -> djinn_core::models::Project {
    let repo = ProjectRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let id = uuid::Uuid::now_v7();
    let path = repo_path.to_str().unwrap().to_string();
    let name = format!("lc-test-{id}");
    repo.create(&name, &path).await.expect("create project")
}

/// Creates a task in `open` status (valid for WorkerRole dispatch).
async fn create_open_task(
    db: &djinn_db::Database,
    project_id: &str,
    epic_id: &str,
) -> djinn_core::models::Task {
    let repo = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let task = repo
        .create_in_project(
            project_id,
            Some(epic_id),
            "lifecycle-test-task",
            "test task description",
            "test design",
            "task",
            2,
            "dev@test",
            None,
            None,
        )
        .await
        .expect("create task");
    // Update acceptance criteria so transition_start doesn't fail
    repo.update(
        &task.id,
        &task.title,
        &task.description,
        &task.design,
        task.priority,
        &task.owner,
        &task.labels,
        r#"[{"description":"write the code","met":false}]"#,
    )
    .await
    .expect("set acceptance criteria");
    task
}

/// Collect the first SlotEvent from the channel with a 3-second deadline.
async fn recv_slot_event(rx: &mut mpsc::Receiver<SlotEvent>) -> SlotEvent {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("slot event should arrive within 3s")
        .expect("slot event channel should stay open")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// Success path: FakeProvider emits a text response (no tools → loop exits
/// cleanly), session reaches `Paused` (WorkerRole preserves the session for
/// resume), and the slot emits `SlotEvent::Free`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_success_path_session_reaches_paused_and_slot_freed() {
    let repo = create_git_repo().await;
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let app_state = agent_context_from_db(db.clone(), cancel.clone());

    let project = register_project(&db, repo.path()).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_open_task(&db, &project.id, &epic.id).await;

    let (event_tx, mut event_rx) = mpsc::channel(4);

    // FakeProvider that calls `submit_work` to end the session cleanly.
    // The worker role expects this finalize tool to exit the reply loop.
    let provider = Arc::new(FakeProvider::tool_call(
        "finalize-1",
        "submit_work",
        serde_json::json!({
            "task_id": task.short_id,
            "summary": "Implementation complete (lifecycle test)"
        }),
    ));

    let params = TaskLifecycleParams {
        task_id: task.id.clone(),
        project_path: project.path.clone(),
        model_id: "synthetic/test-model".to_string(),
        role: role_impl_for(AgentType::Worker),
        app_state: app_state.clone(),
        cancel: cancel.clone(),
        pause: CancellationToken::new(),
        event_tx,
        system_prompt_extensions: String::new(),
        learned_prompt: None,
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        role_verification_command: None,
        provider_override: Some(provider),
    };

    run_task_lifecycle(params)
        .await
        .expect("lifecycle should succeed");

    // ── Assert: slot event is Free ─────────────────────────────────────────
    let event = recv_slot_event(&mut event_rx).await;
    assert!(
        matches!(event, SlotEvent::Free { ref task_id, .. } if task_id == &task.id),
        "expected SlotEvent::Free for task {}, got {event:?}",
        task.id
    );

    // ── Assert: session record exists and is Paused ────────────────────────
    // (WorkerRole.preserves_session = true → session stays open for resume)
    let session_repo = SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let sessions = session_repo
        .list_for_task(&task.id)
        .await
        .expect("list sessions");
    assert!(!sessions.is_empty(), "expected at least one session record");
    let session = &sessions[0];
    assert_eq!(
        session.status.as_str(),
        SessionStatus::Paused.as_str(),
        "worker success path should leave session in Paused state, got {:?}",
        session.status
    );

    // ── Assert: worktree directory exists (preserved for resume) ──────────
    let worktree_path = std::path::Path::new(&project.path)
        .join(".djinn")
        .join("worktrees")
        .join(&task.short_id);
    assert!(
        worktree_path.exists() && worktree_path.is_dir(),
        "worker session should preserve worktree at {worktree_path:?}"
    );
}

/// Provider-failure path: `FailingProvider` causes the reply loop to return
/// `Err(...)`.  The session should reach `Failed` status and the slot emits
/// `SlotEvent::Free`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_provider_failure_session_reaches_failed_and_slot_freed() {
    let repo = create_git_repo().await;
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let app_state = agent_context_from_db(db.clone(), cancel.clone());

    let project = register_project(&db, repo.path()).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_open_task(&db, &project.id, &epic.id).await;

    let (event_tx, mut event_rx) = mpsc::channel(4);

    let provider = Arc::new(FailingProvider::new("injected provider failure for test"));

    let params = TaskLifecycleParams {
        task_id: task.id.clone(),
        project_path: project.path.clone(),
        model_id: "synthetic/test-model".to_string(),
        role: role_impl_for(AgentType::Worker),
        app_state: app_state.clone(),
        cancel: cancel.clone(),
        pause: CancellationToken::new(),
        event_tx,
        system_prompt_extensions: String::new(),
        learned_prompt: None,
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        role_verification_command: None,
        provider_override: Some(provider),
    };

    run_task_lifecycle(params)
        .await
        .expect("lifecycle itself should not error");

    // ── Assert: slot event is Free ─────────────────────────────────────────
    let event = recv_slot_event(&mut event_rx).await;
    assert!(
        matches!(event, SlotEvent::Free { ref task_id, .. } if task_id == &task.id),
        "expected SlotEvent::Free on provider failure, got {event:?}"
    );

    // ── Assert: session record exists and reached Failed ───────────────────
    let session_repo = SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    let sessions = session_repo
        .list_for_task(&task.id)
        .await
        .expect("list sessions");
    assert!(!sessions.is_empty(), "expected at least one session record");
    let session = &sessions[0];
    assert_eq!(
        session.status.as_str(),
        SessionStatus::Failed.as_str(),
        "provider failure should leave session in Failed state, got {:?}",
        session.status
    );
}

/// Worktree cleanup: after a provider failure the lifecycle calls
/// `teardown_worktree`, which removes the worktree directory.  Verify the
/// directory is gone after the lifecycle completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_provider_failure_cleans_up_worktree() {
    let repo = create_git_repo().await;
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let app_state = agent_context_from_db(db.clone(), cancel.clone());

    let project = register_project(&db, repo.path()).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_open_task(&db, &project.id, &epic.id).await;

    let worktree_path = std::path::Path::new(&project.path)
        .join(".djinn")
        .join("worktrees")
        .join(&task.short_id);

    let (event_tx, mut event_rx) = mpsc::channel(4);

    let provider = Arc::new(FailingProvider::new("provider failure for cleanup test"));

    let params = TaskLifecycleParams {
        task_id: task.id.clone(),
        project_path: project.path.clone(),
        model_id: "synthetic/test-model".to_string(),
        role: role_impl_for(AgentType::Worker),
        app_state: app_state.clone(),
        cancel: cancel.clone(),
        pause: CancellationToken::new(),
        event_tx,
        system_prompt_extensions: String::new(),
        learned_prompt: None,
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        role_verification_command: None,
        provider_override: Some(provider),
    };

    run_task_lifecycle(params)
        .await
        .expect("lifecycle should not propagate errors");

    // Drain the slot event (ensures lifecycle is fully done)
    recv_slot_event(&mut event_rx).await;

    // ── Assert: worktree directory removed after failure ───────────────────
    // give the async teardown a brief moment to complete (it's in-line for
    // the worker failure path)
    assert!(
        !worktree_path.exists(),
        "worktree directory should be removed after provider failure, still exists at {worktree_path:?}"
    );
}
