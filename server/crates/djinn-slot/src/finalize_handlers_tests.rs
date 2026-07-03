use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use djinn_core::events::EventBus;
use djinn_core::models::Task;
use djinn_db::Database;
use djinn_provider::catalog::{CatalogService, HealthTracker};
use tokio_util::sync::CancellationToken;

use crate::host::{SlotContext, SlotHostCallbacks};

// A minimal SlotHostCallbacks stub that only implements the trait without
// panicking on paths the auto-submit tests do not exercise.
struct TestCallbacks;

impl SlotHostCallbacks for TestCallbacks {
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::host::ResolvedMcpTools, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err("not implemented in test".into()) })
    }

    fn render_prompt(
        &self,
        _role_name: &str,
        _task: &Task,
        _context_json: &serde_json::Value,
    ) -> String {
        String::new()
    }

    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
        panic!("build_mcp_state not used in auto-submit tests")
    }

    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                error: "not implemented".into(),
            })
        })
    }

    fn resolve_provider_credential<'a>(
        &'a self,
        _provider_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::helpers::ProviderCredential, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err("not implemented in test".into()) })
    }

    fn run_task_dispatch<'a>(
        &'a self,
        _task_id: String,
        _project_path: String,
        _model_id: String,
        _ctx: SlotContext,
        _kill: CancellationToken,
        _pause: CancellationToken,
        _resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn touch_activity_rpc<'a>(
        &'a self,
        _task_id: String,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn flush_session_tokens_rpc<'a>(
        &'a self,
        _session_id: String,
        _tokens_in: i64,
        _tokens_out: i64,
        _cache_read: i64,
        _cache_write: i64,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

fn create_test_db() -> Database {
    Database::open_in_memory().expect("open in-memory test database")
}

fn test_events() -> EventBus {
    EventBus::noop()
}

fn context_with_working_root(db: Database, working_root: PathBuf) -> SlotContext {
    let event_bus = test_events();
    let catalog = CatalogService::new();
    let health_tracker = HealthTracker::default();
    let background_work = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let active_tasks = Arc::new(Mutex::new(std::collections::HashMap::new()));

    SlotContext {
        db,
        event_bus,
        catalog,
        health_tracker,
        background_work_tasks: background_work,
        active_tasks,
        default_project_id: None,
        working_root: Some(working_root),
        coordinator_trigger: None,
        runtime_ops: None,
        repo_graph_ops: None,
        clock: Arc::new(djinn_core::clock::SystemClock::new()),
        callbacks: Arc::new(TestCallbacks),
        tool_dispatcher: None,
    }
}

/// Create a minimal git repository with an initial commit on `main` and a
/// `task/test` branch checked out. A tracked file is created and modified on
/// the task branch so the submission diff fingerprint has a non-empty canonical
/// delta.
async fn make_task_worktree() -> PathBuf {
    let tmp = tempfile::tempdir().expect("create tempdir");
    #[allow(deprecated)]
    let path = tmp.into_path();
    run_git(&path, &["init"]).await;
    run_git(&path, &["config", "user.email", "test@example.com"]).await;
    run_git(&path, &["config", "user.name", "Test"]).await;
    // First commit on main, then rename the default branch to main so the
    // submission diff helper can resolve the base ref.
    std::fs::write(path.join("base.txt"), "base\n").unwrap();
    run_git(&path, &["add", "base.txt"]).await;
    run_git(&path, &["commit", "-m", "initial"]).await;
    run_git(&path, &["branch", "-m", "main"]).await;

    // Create and check out a task branch.
    run_git(&path, &["checkout", "-b", "task/test"]).await;

    // Modify a tracked file on the task branch and commit.
    std::fs::write(path.join("work.txt"), "work content\n").unwrap();
    run_git(&path, &["add", "work.txt"]).await;
    run_git(&path, &["commit", "-m", "work"]).await;

    // Add a dirty untracked change to make sure the helper sees it.
    std::fs::write(path.join("dirty.txt"), "dirty line\n").unwrap();

    path
}

async fn run_git(path: &Path, args: &[&str]) {
    djinn_git::run_git_command(
        path.to_path_buf(),
        args.iter().map(|s| s.to_string()).collect(),
    )
    .await
    .expect("git command succeeded");
}

#[tokio::test]
async fn accepted_auto_submit_persists_shared_helper_fingerprint() {
    let db = create_test_db();
    let worktree_path = make_task_worktree().await;
    let ctx = context_with_working_root(db.clone(), worktree_path.clone());

    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let run_id = uuid::Uuid::now_v7().to_string();
    djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: Some(worktree_path.to_str().unwrap()),
            mirror_ref: None,
        })
        .await
        .expect("create task run");

    let helper_digest = djinn_git::compute_submission_diff_fingerprint(&worktree_path)
        .await
        .expect("helper should compute fingerprint")
        .fingerprint()
        .expect("worktree has a diff")
        .to_string();

    let payload = serde_json::json!({
        "task_id": task.short_id,
        "commit_title": "feat: shared fingerprint",
        "summary": "persist shared helper fingerprint",
        "files_changed": ["work.txt"],
        "remaining_concerns": [],
        "auto_submit_review_metadata": {
            "task_run_id": run_id,
            "trigger_reason": "controlled_termination",
            "diff_fingerprint": "payload-placeholder-fingerprint",
            "verify_source": "worker",
            "verify_run_id": "worker-run-7",
            "verify_timestamp": "2026-07-03T12:00:00.000Z",
            "session_id": "sess-7",
            "model_id": "model-7",
            "no_progress_streak": 3
        }
    });

    let ok = crate::finalize_handlers::process_auto_submit_payload(&payload, &task.id, &ctx).await;
    assert!(ok);

    let records = djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db)
        .list_for_task_run(&run_id)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];

    assert_eq!(record.diff_fingerprint, helper_digest);
    assert_ne!(record.diff_fingerprint, "payload-placeholder-fingerprint");

    assert_eq!(record.trigger_reason, "controlled_termination");
    assert_eq!(record.verify_source.as_deref(), Some("worker"));
    assert_eq!(record.verify_run_id.as_deref(), Some("worker-run-7"));
    assert_eq!(
        record.verify_timestamp.as_deref(),
        Some("2026-07-03T12:00:00.000Z")
    );
    assert_eq!(record.session_id.as_deref(), Some("sess-7"));
    assert_eq!(record.model_id.as_deref(), Some("model-7"));
    assert_eq!(record.no_progress_streak, 3);
    assert!(!record.model_called_submit_work);
}
