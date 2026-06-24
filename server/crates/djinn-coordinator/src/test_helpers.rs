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

pub fn agent_context_from_db(db: Database, _cancel: CancellationToken) -> SlotContext {
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
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
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
        callbacks: Arc::new(NoopCallbacks),
    }
}

pub async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    let event_bus = EventBus::noop();
    let repo = djinn_db::ProjectRepository::new(db.clone(), event_bus);
    let uuid = uuid::Uuid::now_v7().simple();
    repo.create(
        &format!("test-project-{uuid}"),
        &format!("owner-{uuid}"),
        &format!("repo-{uuid}"),
    )
    .await
    .expect("create project")
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
        repo_graph_ops: None,
        runtime_ops: None,
        cargo_target_runs_root: None,
        mirror: None,
        rpc_registry: None,
        default_project_id: None,
        reconciliation_sweep: crate::context::ReconciliationSweepConfig::default(),
    }
}
