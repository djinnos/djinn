//! Integration-style lifecycle tests for `run_task_lifecycle`.
//!
//! Uses the provider-injection seam added to `TaskLifecycleParams` so no real
//! LLM calls are made.  Each test spins up a real (tmpdir) git repository
//! because `prepare_worktree` needs one; the git ops are fast and keep total
//! test time well under 500ms.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AgentType;
use crate::actors::slot::SlotEvent;
use crate::actors::slot::lifecycle::{TaskLifecycleParams, run_task_lifecycle};
use crate::roles::role_impl_for;
use crate::test_helpers::{
    CapturingProvider, FailingProvider, FakeProvider, agent_context_from_db, create_test_db,
    create_test_epic,
};
use djinn_core::models::SessionStatus;
use djinn_db::AgentCreateInput;
use djinn_db::{ProjectRepository, SessionRepository, TaskRepository};

// ─── Git repo helpers ─────────────────────────────────────────────────────────

/// Creates a temporary git repository with a single commit on `main`.
async fn create_git_repo() -> TempDir {
    let tmp = crate::test_helpers::test_tempdir("djinn-lifecycle-");
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

async fn create_specialist_worker(
    db: &djinn_db::Database,
    project_id: &str,
    name: &str,
    mcp_servers: &str,
) {
    let repo = djinn_db::AgentRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    repo.create_for_project(
        project_id,
        AgentCreateInput {
            name,
            base_role: "worker",
            description: "specialist lifecycle test agent",
            system_prompt_extensions: "",
            model_preference: None,
            verification_command: None,
            mcp_servers: Some(mcp_servers),
            skills: Some("[]"),
            is_default: false,
        },
    )
    .await
    .expect("create specialist agent");
}

fn tool_names(tools: &[serde_json::Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|tool| {
            tool.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .or_else(|| tool.get("name").and_then(|v| v.as_str()))
                .map(str::to_string)
        })
        .collect()
}

/// Collect the first SlotEvent from the channel with a 3-second deadline.
async fn recv_slot_event(rx: &mut mpsc::Receiver<SlotEvent>) -> SlotEvent {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("slot event should arrive within 3s")
        .expect("slot event channel should stay open")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_specialist_appends_discovered_mcp_tools_to_provider_tool_list() {
    let repo = create_git_repo().await;
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let app_state = agent_context_from_db(db.clone(), cancel.clone());

    let project = register_project(&db, repo.path()).await;
    let epic = create_test_epic(&db, &project.id).await;
    create_specialist_worker(&db, &project.id, "knowledge-harvester", r#"["web"]"#).await;
    let task = create_open_task(&db, &project.id, &epic.id).await;
    let task_repo = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    task_repo
        .update_agent_type(&task.id, Some("knowledge-harvester"))
        .await
        .expect("set specialist agent_type");

    let provider = Arc::new(CapturingProvider::tool_call(
        "finalize-1",
        "submit_work",
        serde_json::json!({
            "task_id": task.short_id,
            "summary": "specialist session complete"
        }),
    ));
    let mcp_registry = crate::mcp_client::McpToolRegistry::with_dispatch(
        [("web_search".to_string(), "web".to_string())],
        vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"]
                }
            }
        })],
        |_tool_name, _arguments| Ok(serde_json::json!({"ok": true})),
    );

    let (event_tx, mut event_rx) = mpsc::channel(4);
    run_task_lifecycle(TaskLifecycleParams {
        task_id: task.id.clone(),
        project_path: project.path.clone(),
        model_id: "synthetic/test-model".to_string(),
        role: role_impl_for(AgentType::Architect),
        app_state: app_state.clone(),
        cancel: cancel.clone(),
        pause: CancellationToken::new(),
        event_tx,
        system_prompt_extensions: String::new(),
        learned_prompt: None,
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        role_verification_command: None,
        mcp_registry_override: Some(mcp_registry),
        provider_override: Some(provider.clone()),
    })
    .await
    .expect("lifecycle should succeed");

    recv_slot_event(&mut event_rx).await;
    let captured = provider.captured_tools();
    assert!(!captured.is_empty(), "provider should observe a tool list");
    let names = tool_names(&captured[0]);
    assert!(
        names.contains(&"submit_work".to_string()),
        "runtime role should resolve specialist base role toolset"
    );
    assert!(
        names.contains(&"web_search".to_string()),
        "discovered MCP tool should be appended"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_default_session_keeps_builtins_only_without_mcp_servers() {
    let repo = create_git_repo().await;
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let app_state = agent_context_from_db(db.clone(), cancel.clone());

    let project = register_project(&db, repo.path()).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_open_task(&db, &project.id, &epic.id).await;

    let provider = Arc::new(CapturingProvider::tool_call(
        "finalize-1",
        "submit_work",
        serde_json::json!({
            "task_id": task.short_id,
            "summary": "default session complete"
        }),
    ));

    let (event_tx, mut event_rx) = mpsc::channel(4);
    run_task_lifecycle(TaskLifecycleParams {
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
        mcp_registry_override: None,
        provider_override: Some(provider.clone()),
    })
    .await
    .expect("lifecycle should succeed");

    recv_slot_event(&mut event_rx).await;
    let captured = provider.captured_tools();
    assert!(!captured.is_empty(), "provider should observe a tool list");
    let names = tool_names(&captured[0]);
    assert!(names.contains(&"submit_work".to_string()));
    assert!(!names.contains(&"web_search".to_string()));
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
        mcp_registry_override: None,
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
        mcp_registry_override: None,
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

/// Worktree preservation: after a provider failure the lifecycle preserves the
/// worktree so the next retry session can reuse the target/ build cache.
/// Real teardown happens on task close/merge (task_merge.rs) or via
/// purge_all_worktrees on execution restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_provider_failure_preserves_worktree_for_retry() {
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
        mcp_registry_override: None,
        provider_override: Some(provider),
    };

    run_task_lifecycle(params)
        .await
        .expect("lifecycle should not propagate errors");

    // Drain the slot event (ensures lifecycle is fully done)
    recv_slot_event(&mut event_rx).await;

    // ── Assert: worktree directory preserved for retry ─────────────────────
    // Failed sessions now preserve the worktree so the next session can
    // reuse the target/ build cache instead of rebuilding from scratch.
    assert!(
        worktree_path.exists(),
        "worktree directory should be preserved after provider failure for retry, but was removed at {worktree_path:?}"
    );
}

// ─── ADR-050 Chunk C cold-start canonical-graph warming ──────────────────────

/// Test double for `CanonicalGraphWarmer` that records each `warm` invocation
/// and lets the test pick whether to return Ok or Err.
#[derive(Clone, Default)]
struct RecordingWarmer {
    calls: Arc<std::sync::Mutex<Vec<(String, std::path::PathBuf)>>>,
    fail: bool,
}

impl RecordingWarmer {
    fn ok() -> Self {
        Self::default()
    }
    fn failing() -> Self {
        Self {
            calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            fail: true,
        }
    }
    fn calls(&self) -> Vec<(String, std::path::PathBuf)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl crate::context::CanonicalGraphWarmer for RecordingWarmer {
    async fn warm(&self, project_id: &str, project_root: &Path) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push((project_id.to_string(), project_root.to_path_buf()));
        if self.fail {
            Err("synthetic warmer failure".into())
        } else {
            Ok(())
        }
    }
}

/// Worker dispatch must NOT call `CanonicalGraphWarmer::warm`.  Warming
/// happens only on architect dispatch (see `lifecycle_architect_*` tests);
/// workers, reviewers, planners, and lead tolerate a stale skeleton from
/// whatever the most recent architect warm left in the cache.  This
/// avoids wedging the dispatcher behind a cold-cache SCIP rebuild that
/// can take tens of minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_worker_dispatch_does_not_warm_canonical_graph_cache() {
    let repo = create_git_repo().await;
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let mut app_state = agent_context_from_db(db.clone(), cancel.clone());
    let warmer = Arc::new(RecordingWarmer::ok());
    app_state.canonical_graph_warmer = Some(warmer.clone());

    let project = register_project(&db, repo.path()).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_open_task(&db, &project.id, &epic.id).await;

    let provider = Arc::new(FakeProvider::tool_call(
        "finalize-1",
        "submit_work",
        serde_json::json!({
            "task_id": task.short_id,
            "summary": "warm cache test"
        }),
    ));

    let (event_tx, mut event_rx) = mpsc::channel(4);
    run_task_lifecycle(TaskLifecycleParams {
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
        mcp_registry_override: None,
        provider_override: Some(provider),
    })
    .await
    .expect("lifecycle should succeed");
    recv_slot_event(&mut event_rx).await;

    let calls = warmer.calls();
    assert_eq!(
        calls.len(),
        0,
        "worker dispatch must NOT warm the canonical graph (would wedge dispatcher behind SCIP rebuild), got {calls:?}"
    );

    // Workers must NOT have their working_root pinned to the index tree —
    // their tools still resolve against the per-task worktree.
    assert!(
        app_state.working_root.is_none(),
        "worker dispatch must not pin working_root, was {:?}",
        app_state.working_root
    );
}

/// A configured warmer that would fail must not affect worker dispatch
/// at all: since workers no longer call the warmer, the failing warmer
/// should never be invoked and the session should still complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_worker_ignores_failing_canonical_graph_warmer() {
    let repo = create_git_repo().await;
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let mut app_state = agent_context_from_db(db.clone(), cancel.clone());
    let warmer = Arc::new(RecordingWarmer::failing());
    app_state.canonical_graph_warmer = Some(warmer.clone());

    let project = register_project(&db, repo.path()).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_open_task(&db, &project.id, &epic.id).await;

    let provider = Arc::new(FakeProvider::tool_call(
        "finalize-1",
        "submit_work",
        serde_json::json!({
            "task_id": task.short_id,
            "summary": "warm-failure tolerated"
        }),
    ));

    let (event_tx, mut event_rx) = mpsc::channel(4);
    run_task_lifecycle(TaskLifecycleParams {
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
        mcp_registry_override: None,
        provider_override: Some(provider),
    })
    .await
    .expect("lifecycle should not propagate warming failure");
    recv_slot_event(&mut event_rx).await;

    assert_eq!(
        warmer.calls().len(),
        0,
        "worker dispatch must not invoke the warmer at all, even a failing one"
    );
    // The session must have made it past the warming step and reached the
    // provider — confirm by checking SessionRepository for at least one row.
    let sessions = SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
        .list_for_task(&task.id)
        .await
        .expect("list sessions");
    assert!(
        !sessions.is_empty(),
        "lifecycle must have created a session despite warming failure"
    );
}
