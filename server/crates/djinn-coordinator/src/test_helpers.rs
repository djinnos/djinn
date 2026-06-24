//! Test helpers for djinn-coordinator tests.
//!
//! Mirrors `djinn-agent::test_helpers` for coordinator test fixtures.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_db::{Database, EpicCreateInput, EpicRepository, ProjectRepository, TaskRepository};

pub fn test_events() -> EventBus {
    EventBus::noop()
}

pub fn test_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = test_tmp_base();
    std::fs::create_dir_all(&base).expect("create test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create test tempdir")
}

fn test_tmp_base() -> PathBuf {
    if let Ok(base) = std::env::var("CARGO_TARGET_TMPDIR") {
        let base = PathBuf::from(base).join("djinn-coordinator");
        if base.is_relative() {
            std::env::current_dir().expect("current dir").join(base)
        } else {
            base
        }
    } else {
        std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("test-tmp")
    }
}

pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

pub async fn create_test_project(db: &Database) -> djinn_core::models::Project {
    let events = test_events();
    let repo = ProjectRepository::new(db.clone(), events);
    let id = uuid::Uuid::now_v7();
    let compact_id = id.simple().to_string();
    let name = format!("test-project-{compact_id}");
    let repo_slug = format!("test-project-{}", &compact_id[..23]);
    let project = repo
        .create(&name, "test", &repo_slug)
        .await
        .expect("create project");
    // Satisfy the coordinator's readiness gate so existing tests can dispatch
    // without threading a full devcontainer pipeline. Keep both readiness
    // representations populated: legacy project image columns for older
    // callers, catalog-image selection for dispatch, and graph freshness rows
    // for both repo-level and per-workspace checks.
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
    // Also satisfy the catalog-image readiness path used by dispatch.
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

pub async fn create_test_epic(db: &Database, project_id: &str) -> djinn_core::models::Epic {
    let events = test_events();
    let repo = EpicRepository::new(db.clone(), events);
    repo.create_for_project(
        project_id,
        EpicCreateInput {
            title: "Test Epic",
            description: "A test epic",
            emoji: "🧪",
            color: "blue",
            owner: "test-owner",
            memory_refs: None,
            status: None,
            auto_breakdown: None,
            originating_adr_id: None,
            blocked_by: None,
        },
    )
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
    repo.create_in_project(
        project_id,
        Some(epic_id),
        "Test Task",
        "A test task",
        "",
        "task",
        1,
        "test-owner",
        None,
        None,
    )
    .await
    .expect("create task")
}

pub fn agent_context_from_db(
    db: Database,
    _cancel: tokio_util::sync::CancellationToken,
) -> super::context::AgentContext {
    super::context::AgentContext {
        db: db.clone(),
        event_bus: EventBus::noop(),
        git_actors: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        background_work_tasks: Arc::new(std::sync::Mutex::new(HashSet::new())),
        role_registry: Arc::new(super::roles_compat::RoleRegistry::new()),
        health_tracker: djinn_provider::catalog::health::HealthTracker::new(),
        file_time: Arc::new(super::file_time::FileTime::new()),
        lsp: djinn_lsp::LspManager::new(),
        catalog: djinn_provider::catalog::CatalogService::new(),
        coordinator: Arc::new(tokio::sync::Mutex::new(None)),
        active_tasks: Arc::new(std::sync::Mutex::new(HashMap::new())),
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

/// Construct a [`djinn_slot::host::SlotContext`] from a coordinator
/// `AgentContext` for tests that need to spawn a real slot pool.
pub fn slot_context_from_agent(
    ctx: &super::context::AgentContext,
) -> djinn_slot::host::SlotContext {
    djinn_slot::host::SlotContext {
        db: ctx.db.clone(),
        event_bus: ctx.event_bus.clone(),
        catalog: ctx.catalog.clone(),
        health_tracker: ctx.health_tracker.clone(),
        background_work_tasks: ctx.background_work_tasks.clone(),
        active_tasks: ctx.active_tasks.clone(),
        default_project_id: ctx.default_project_id.clone(),
        working_root: ctx.working_root.clone(),
        coordinator_trigger: None,
        runtime_ops: ctx.runtime_ops.clone(),
        repo_graph_ops: ctx.repo_graph_ops.clone(),
        callbacks: Arc::new(NoopSlotCallbacks),
    }
}

// ── Stub host callbacks ────────────────────────────────────────────────

struct NoopToolRegistryHandle;

impl djinn_slot::host::ToolRegistryHandle for NoopToolRegistryHandle {
    fn dispatch_tool<'a>(
        &'a self,
        _tool_name: &'a str,
        _arguments: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(async { Err("noop tool registry".into()) })
    }
}

/// Stub [`djinn_slot::host::SlotHostCallbacks`] for tests that don't
/// exercise the host callback paths.
struct NoopSlotCallbacks;

impl djinn_slot::host::SlotHostCallbacks for NoopSlotCallbacks {
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Ok(djinn_slot::host::ResolvedMcpTools {
                tool_definitions: Vec::new(),
                registry_handle: Arc::new(NoopToolRegistryHandle),
            })
        })
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
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }

    fn build_mcp_state(
        &self,
        _ctx: &djinn_slot::host::SlotContext,
    ) -> djinn_control_plane::McpState {
        panic!("NoopSlotCallbacks: build_mcp_state not implemented")
    }

    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse::new(
                "noop: no project",
            ))
        })
    }

    fn resolve_provider_credential<'a>(
        &'a self,
        _provider_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<djinn_slot::helpers::ProviderCredential, String>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err("noop: no credentials".into()) })
    }

    fn run_task_dispatch<'a>(
        &'a self,
        _task_id: String,
        _project_path: String,
        _model_id: String,
        _ctx: djinn_slot::host::SlotContext,
        _kill: tokio_util::sync::CancellationToken,
        _pause: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}
