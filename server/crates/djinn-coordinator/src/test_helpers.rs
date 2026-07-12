//! Test utilities for djinn-coordinator tests.
//!
//! Mirrors the subset of `djinn_slot::test_helpers` and
//! `djinn_agent::test_helpers` that coordinator tests need.
//! Returns [`SlotContext`] (from djinn-slot) rather than `AgentContext`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_slot::host::SlotContext;
use djinn_slot::reply_loop::CompactionCriticalSection;

pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("failed to create tempdir")
}

pub fn test_persistent_dir(prefix: &str) -> PathBuf {
    test_tempdir(prefix).keep()
}

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("open in-memory test database")
}

pub fn agent_context_from_db(db: Database, cancel: CancellationToken) -> SlotContext {
    agent_context_from_db_with_clock(db, cancel, Arc::new(djinn_core::clock::SystemClock::new()))
}

/// Like [`agent_context_from_db`] but with an injectable [`djinn_core::clock::Clock`]
/// so tests can advance monotonic time deterministically (e.g. to drive the
/// stall-timeout recovery path without sleeping).
pub fn agent_context_from_db_with_clock(
    db: Database,
    _cancel: CancellationToken,
    clock: Arc<dyn djinn_core::clock::Clock>,
) -> SlotContext {
    let event_bus = EventBus::noop();
    let catalog = CatalogService::new();
    let health_tracker = HealthTracker::default();
    let background_work = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // No-op host callbacks for tests
    struct NoopCallbacks;
    impl djinn_slot::host::SlotHostCallbacks for NoopCallbacks {
        fn interrupt_paused_worker_session<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }
        fn resolve_mcp_tools<'a>(
            &'a self,
            _worktree_path: &'a str,
            _role_name: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err("not implemented in test".into()) })
        }
        fn render_prompt(
            &self,
            _role_name: &str,
            _task: &djinn_core::models::Task,
            _context_json: &serde_json::Value,
        ) -> String {
            String::new()
        }
        fn initial_user_message<'a>(
            &'a self,
            _task_id: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async { String::new() })
        }
        fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
            panic!(
                "build_mcp_state not implemented in test NoopCallbacks; \
                 override via a custom SlotHostCallbacks impl if your test needs McpState"
            )
        }
        fn require_project_id_for_task_ops<'a>(
            &'a self,
            _project: &'a str,
            _ctx: &'a SlotContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            String,
                            djinn_control_plane::tools::task_tools::ErrorResponse,
                        >,
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
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<djinn_slot::helpers::ProviderCredential, String>,
                    > + Send
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
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
        fn touch_activity_rpc<'a>(
            &'a self,
            _task_id: String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
        fn flush_session_tokens_rpc<'a>(
            &'a self,
            _session_id: String,
            _tokens_in: i64,
            _tokens_out: i64,
            _cache_read: i64,
            _cache_write: i64,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    SlotContext {
        db,
        event_bus,
        catalog,
        health_tracker,
        background_work_tasks: background_work,
        active_tasks,
        default_project_id: None,
        working_root: None,
        coordinator_trigger: None,
        runtime_ops: None,
        repo_graph_ops: None,
        clock,
        callbacks: Arc::new(NoopCallbacks),
        tool_dispatcher: None,
        compaction_cs: CompactionCriticalSection::new(),
    }
}

pub async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    let event_bus = EventBus::noop();
    let repo = djinn_db::ProjectRepository::new(db.clone(), event_bus);
    let uuid = uuid::Uuid::now_v7().simple();
    let project = repo
        .create(
            &format!("test-project-{uuid}"),
            &format!("owner-{uuid}"),
            &format!("repo-{uuid}"),
        )
        .await
        .expect("create project");
    // Satisfy the coordinator's readiness gate so existing tests can dispatch
    // without threading a full devcontainer pipeline.
    let image = djinn_db::ProjectImage {
        tag: Some(format!(
            "test-registry/djinn-project-{}:testhash",
            &project.id
        )),
        hash: Some("testhash".into()),
        status: djinn_db::ProjectImageStatus::READY.into(),
        last_error: None,
    };
    let _ = repo.set_project_image(&project.id, &image).await;
    let image_repo = djinn_db::ImageRepository::new(db.clone());
    let image_id = format!(
        "ci-ready-{}",
        &uuid::Uuid::now_v7().simple().to_string()[..16]
    );
    let image_name = format!("ci-ready-{}", &image_id[..8]);
    let _ = image_repo
        .create(
            &image_id,
            &image_name,
            Some("ready test image"),
            r#"{"schema_version":1}"#,
        )
        .await;
    let _ = image_repo
        .mark_ready(
            &image_id,
            image
                .tag
                .as_deref()
                .unwrap_or("test-registry/djinn-test:testhash"),
            Some("sha256:testhash"),
        )
        .await;
    let _ = image_repo
        .set_project_image(&project.id, Some(&image_id))
        .await;
    let cache_repo = djinn_db::RepoGraphCacheRepository::new(db.clone());
    let _ = cache_repo
        .upsert(djinn_db::RepoGraphCacheInsert {
            project_id: &project.id,
            commit_sha: "test-commit",
            graph_blob: b"test-graph",
        })
        .await;
    let _ = djinn_db::ProjectWorkspaceGraphRepository::new(db.clone())
        .upsert(djinn_db::ProjectWorkspaceGraphUpsert {
            project_id: &project.id,
            workspace_slug: "root",
            commit_sha: "test-commit",
            status: "ready",
        })
        .await;
    project
}

/// Build a [`CoordinatorContext`] for tests that exercise coordinator-owned
/// health/doctor functions (which take `&CoordinatorContext`, not
/// `&SlotContext`).
pub fn coordinator_context_from_db(
    db: Database,
    _cancel: CancellationToken,
) -> crate::context::CoordinatorContext {
    let event_bus = EventBus::noop();
    let catalog = CatalogService::new();
    let health_tracker = HealthTracker::default();
    let background_work = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let role_registry = Arc::new(crate::roles::RoleRegistry::new());

    crate::context::CoordinatorContext {
        db,
        event_bus,
        git_actors: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        background_work_tasks: background_work,
        role_registry,
        health_tracker,
        file_time: Arc::new(crate::file_time::FileTime::new()),
        lsp: djinn_lsp::LspManager::new(),
        catalog,
        active_tasks,
        task_ops_project_path_override: None,
        working_root: None,
        graph_warmer: None,
        warm_job_guard: None,
        repo_graph_ops: None,
        runtime_ops: None,
        cargo_target_runs_root: None,
        mirror: None,
        rpc_registry: None,
        default_project_id: None,
        reconciliation_sweep: crate::context::ReconciliationSweepConfig::default(),
        cache_cleanup: crate::context::CacheCleanupConfig::default(),
    }
}
