//! Shared test utilities for djinn-slot tests.

use djinn_core::events::EventBus;
use djinn_db::Database;
use tokio_util::sync::CancellationToken;

use crate::host::SlotContext;

pub fn create_test_db() -> Database {
    Database::new_ephemeral()
}

pub fn test_events() -> EventBus {
    EventBus::new(|_| {})
}

pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("failed to create tempdir")
}

pub fn test_path(prefix: &str) -> std::path::PathBuf {
    test_tempdir(prefix).into_path()
}

pub fn agent_context_from_db(db: Database, _cancel: CancellationToken) -> SlotContext {
    let event_bus = test_events();
    let catalog = djinn_provider::catalog::CatalogService::new();
    let health_tracker = djinn_provider::catalog::HealthTracker::default();
    let background_work =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let active_tasks = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    // No-op host callbacks for tests
    struct NoopCallbacks;
    impl crate::host::SlotHostCallbacks for NoopCallbacks {
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
            djinn_control_plane::McpState::default()
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
            _kill: tokio_util::sync::CancellationToken,
            _pause: tokio_util::sync::CancellationToken,
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
        callbacks: std::sync::Arc::new(NoopCallbacks),
    }
}

pub async fn create_test_project(db: &Database) -> djinn_db::Project {
    let event_bus = test_events();
    let repo = djinn_db::ProjectRepository::new(db.clone(), event_bus);
    repo.create("test-project", "Test Project", "main")
        .await
        .expect("create project")
}
