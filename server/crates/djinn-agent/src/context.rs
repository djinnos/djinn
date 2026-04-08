use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use djinn_git::{GitActorHandle, GitError};
use djinn_mcp::{
    McpState, bridge,
    bridge::{
        CycleGroup, EdgeEntry, GraphDiff, ImpactResult, NeighborsResult, OrphanEntry, PathResult,
        RankedNode, RepoGraphOps, SearchHit, SymbolDescription,
    },
    tools::task_tools::ErrorResponse,
};
use tokio::sync::Mutex;

use crate::actors::coordinator::{CoordinatorHandle, VerificationTracker};
use crate::file_time::FileTime;
use crate::lsp::LspManager;
use crate::roles::RoleRegistry;
use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_provider::catalog::{CatalogService, HealthTracker};

/// Shared tracker for per-task last-activity timestamps (unix seconds).
/// Used by stall detection to kill sessions that stop producing tokens.
pub type ActivityTracker = Arc<std::sync::Mutex<HashMap<String, Arc<AtomicU64>>>>;

/// Cross-crate hook into the server's `ensure_canonical_graph` machinery so
/// the agent lifecycle can warm the canonical-main graph cache before any
/// role (worker, reviewer, planner, lead, architect) starts its session.
///
/// `djinn-agent` does not depend on the server crate, so the concrete
/// implementation is wired in `server::AppState::agent_context()`.  Tests in
/// this crate leave the warmer as `None`, which makes the warming call a
/// no-op.
#[async_trait]
pub trait CanonicalGraphWarmer: Send + Sync {
    /// Resolve `(project_id, project_root)` to the canonical graph cache,
    /// building it if necessary.  Best-effort: callers must treat any error
    /// as non-fatal and let the agent continue without a warm skeleton.
    async fn warm(&self, project_id: &str, project_root: &Path) -> Result<(), String>;
}

/// Subset of application state required by agent lifecycle, coordinator, and
/// slot code.  Cheaply cloneable — all fields are either `Clone` or wrapped in
/// `Arc`.
///
/// Construct via [`crate::server::AppState::agent_context()`] at the server
/// boundary, or via [`crate::test_helpers::agent_context_from_db()`] in tests.
#[derive(Clone)]
pub struct AgentContext {
    pub db: Database,
    pub event_bus: EventBus,
    pub git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
    pub verifying_tasks: VerificationTracker,
    pub role_registry: Arc<RoleRegistry>,
    pub health_tracker: HealthTracker,
    pub file_time: Arc<FileTime>,
    pub lsp: LspManager,
    pub catalog: CatalogService,
    pub coordinator: Arc<tokio::sync::Mutex<Option<CoordinatorHandle>>>,
    pub active_tasks: ActivityTracker,
    pub task_ops_project_path_override: Option<PathBuf>,
    /// Per-ADR-050 working root for code-reading tools (`read`, `shell`, `lsp`,
    /// `code_graph`).  When `Some`, the dispatch layer routes those tools
    /// against this path instead of the per-task worktree.  Used by the
    /// Architect and Chat surfaces to read against the canonical
    /// `.djinn/worktrees/_index/` checkout pinned to `origin/main`.  Workers,
    /// reviewers, planners, and lead leave this `None` so their tools continue
    /// to resolve against their task worktree.
    pub working_root: Option<PathBuf>,
    /// Optional hook into the server's `ensure_canonical_graph` machinery.
    /// When `Some`, the slot lifecycle calls it before starting the agent
    /// runtime so workers/reviewers/planners/lead receive a freshly rendered
    /// canonical-main repo-map note via the standard note pipeline.  When
    /// `None` (tests, off-server contexts) the warming call is skipped.
    pub canonical_graph_warmer: Option<Arc<dyn CanonicalGraphWarmer>>,
    /// Real `RepoGraphOps` implementation injected at the server boundary
    /// (typically `RepoGraphBridge` wrapping `AppState`).  When `Some`, the
    /// agent bridge routes `code_graph` tool calls through it — the same path
    /// the external MCP server uses.  When `None` (tests, off-server
    /// contexts) the bridge falls back to a stub that returns errors from
    /// every method.
    ///
    /// `djinn-agent` cannot depend on the server crate (per ADR-047), so the
    /// concrete `RepoGraphBridge` is wired in `server::AppState::agent_context()`.
    pub repo_graph_ops: Option<Arc<dyn RepoGraphOps>>,
}

impl AgentContext {
    /// Returns the working root for code-reading tool dispatch (read, shell,
    /// lsp, code_graph).  When `working_root` is `Some` it takes precedence
    /// over the supplied fallback (typically the worker's worktree path).
    pub fn working_root_for(&self, fallback: &Path) -> PathBuf {
        match self.working_root.as_deref() {
            Some(p) => p.to_path_buf(),
            None => fallback.to_path_buf(),
        }
    }
}

struct AgentSyncOps;

#[async_trait]
impl bridge::SyncOps for AgentSyncOps {
    async fn enable_project(&self, _: &str) -> Result<(), String> {
        Err("sync not available in djinn-agent task-mutation bridge".into())
    }

    async fn disable_project(&self, _: &str) -> Result<(), String> {
        Err("sync not available in djinn-agent task-mutation bridge".into())
    }

    async fn delete_remote_branch(&self, _: &str, _: &Path) -> Result<(), String> {
        Err("sync not available in djinn-agent task-mutation bridge".into())
    }

    async fn export_all(&self, _: Option<&str>) -> Vec<bridge::SyncResult> {
        vec![]
    }

    async fn import_all(&self) -> Vec<bridge::SyncResult> {
        vec![]
    }

    async fn status(&self) -> Vec<bridge::ChannelStatus> {
        vec![]
    }
}

struct AgentRuntimeOps {
    db: Database,
    event_bus: EventBus,
    health_tracker: HealthTracker,
}

#[async_trait]
impl bridge::RuntimeOps for AgentRuntimeOps {
    async fn apply_settings(&self, _: &djinn_core::models::DjinnSettings) -> Result<(), String> {
        Ok(())
    }

    async fn reset_runtime_settings(&self) {}

    async fn persist_model_health_state(&self) {
        use djinn_db::SettingsRepository;
        const MODEL_HEALTH_STATE_KEY: &str = "model_health.state";
        let repo = SettingsRepository::new(self.db.clone(), self.event_bus.clone());
        let snapshot = self.health_tracker.all_health();
        match serde_json::to_string(&snapshot) {
            Ok(raw) => {
                if let Err(e) = repo.set(MODEL_HEALTH_STATE_KEY, &raw).await {
                    tracing::warn!(error = %e, "failed to persist model health state");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to serialize model health state"),
        }
    }

    async fn purge_worktrees(&self) {}
}

/// Fallback `RepoGraphOps` implementation used when `AgentContext` is built
/// without an injected real bridge (tests and off-server contexts).  Every
/// method returns an error so callers see a clear "not available" signal
/// instead of a silent empty result.  Production builds always inject the
/// real `RepoGraphBridge` via `server::AppState::agent_context()`.
struct StubRepoGraphOps;

#[async_trait]
impl RepoGraphOps for StubRepoGraphOps {
    async fn neighbors(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn ranked(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<RankedNode>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn implementations(&self, _: &str, _: &str) -> Result<Vec<String>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn impact(
        &self,
        _: &str,
        _: &str,
        _: usize,
        _: Option<&str>,
    ) -> Result<ImpactResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn search(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<SearchHit>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn cycles(&self, _: &str, _: Option<&str>, _: usize) -> Result<Vec<CycleGroup>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn orphans(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn path(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<usize>,
    ) -> Result<Option<PathResult>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn edges(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<EdgeEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn diff(&self, _: &str, _: Option<&str>) -> Result<Option<GraphDiff>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn describe(&self, _: &str, _: &str) -> Result<Option<SymbolDescription>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn status(&self, _: &str) -> Result<bridge::GraphStatus, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
}

struct AgentGitOps {
    git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
}

#[async_trait]
impl bridge::GitOps for AgentGitOps {
    async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.git_actors.lock().await;
        djinn_git::get_or_spawn(&mut map, path)
    }
}

#[async_trait]
impl bridge::LspOps for LspManager {
    async fn warnings(&self) -> Vec<bridge::LspWarning> {
        self.warnings()
            .await
            .into_iter()
            .map(|warning| bridge::LspWarning {
                server: warning.server,
                message: warning.message,
            })
            .collect()
    }
}

impl AgentContext {
    /// Build the public djinn-mcp state bridge expected by the shared task-mutation ops.
    ///
    /// This keeps djinn-agent on the supported ADR-041 seam: shared business logic stays in
    /// djinn-mcp, while agent adapters reuse their existing database/event-bus/project resolver
    /// dependencies instead of reconstructing MCP internals locally.
    pub fn to_mcp_state(&self) -> McpState {
        McpState::new(
            self.db.clone(),
            self.event_bus.clone(),
            self.catalog.clone(),
            self.health_tracker.clone(),
            "agent".to_owned(),
            None,
            None,
            Arc::new(self.lsp.clone()),
            Arc::new(AgentSyncOps),
            Arc::new(AgentRuntimeOps {
                db: self.db.clone(),
                event_bus: self.event_bus.clone(),
                health_tracker: self.health_tracker.clone(),
            }),
            Arc::new(AgentGitOps {
                git_actors: self.git_actors.clone(),
            }),
            self.repo_graph_ops
                .clone()
                .unwrap_or_else(|| Arc::new(StubRepoGraphOps) as Arc<dyn RepoGraphOps>),
        )
    }

    /// Resolve a project path through the same public djinn-mcp contract used by external
    /// task-mutation callers.
    pub async fn require_project_id_for_task_ops(
        &self,
        project: &str,
    ) -> Result<String, ErrorResponse> {
        let project = self
            .task_ops_project_path_override
            .as_deref()
            .and_then(|override_path| override_path.to_str())
            .filter(|path| !path.is_empty())
            .unwrap_or(project);
        let server = djinn_mcp::server::DjinnMcpServer::new(self.to_mcp_state());
        match server.require_project_id_public(project).await {
            Ok(project_id) => Ok(project_id),
            Err(_initial_error)
                if project
                    != project
                        .trim_end_matches(std::path::MAIN_SEPARATOR)
                        .trim_end_matches('/') =>
            {
                server
                    .require_project_id_public(
                        project
                            .trim_end_matches(std::path::MAIN_SEPARATOR)
                            .trim_end_matches('/'),
                    )
                    .await
            }
            Err(error) => {
                let repo =
                    djinn_db::ProjectRepository::new(self.db.clone(), self.event_bus.clone());
                repo.resolve_id_by_path_fuzzy(project)
                    .await
                    .map_err(|repo_error| ErrorResponse::new(repo_error.to_string()))?
                    .ok_or(error)
            }
        }
    }

    /// Get or spawn a `GitActorHandle` for the given project path.
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.git_actors.lock().await;
        djinn_git::get_or_spawn(&mut map, path)
    }

    /// Register a task as having an in-flight verification pipeline.
    pub fn register_verification(&self, task_id: &str) {
        self.verifying_tasks
            .lock()
            .expect("poisoned")
            .insert(task_id.to_string());
    }

    /// Deregister a task's verification pipeline (completed or crashed).
    pub fn deregister_verification(&self, task_id: &str) {
        self.verifying_tasks
            .lock()
            .expect("poisoned")
            .remove(task_id);
    }

    /// Check whether a task has a live verification pipeline.
    pub fn has_verification(&self, task_id: &str) -> bool {
        self.verifying_tasks
            .lock()
            .expect("poisoned")
            .contains(task_id)
    }

    /// Register a task as active and return the shared timestamp atomic.
    /// The atomic is initialized to the current unix timestamp.
    pub fn register_activity(&self, task_id: &str) -> Arc<AtomicU64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ts = Arc::new(AtomicU64::new(now));
        self.active_tasks
            .lock()
            .expect("poisoned")
            .insert(task_id.to_string(), ts.clone());
        ts
    }

    /// Update the activity timestamp for a task to now.
    pub fn touch_activity(&self, task_id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(ts) = self.active_tasks.lock().expect("poisoned").get(task_id) {
            ts.store(now, Ordering::Relaxed);
        }
    }

    /// Return seconds since last activity touch, or `None` if not registered.
    pub fn idle_seconds(&self, task_id: &str) -> Option<u64> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let guard = self.active_tasks.lock().expect("poisoned");
        let ts = guard.get(task_id)?;
        let last = ts.load(Ordering::Relaxed);
        Some(now.saturating_sub(last))
    }

    /// Deregister a task's activity tracker.
    pub fn deregister_activity(&self, task_id: &str) {
        self.active_tasks.lock().expect("poisoned").remove(task_id);
    }

    /// Get the current coordinator handle, if one is running.
    pub async fn coordinator(&self) -> Option<CoordinatorHandle> {
        self.coordinator.lock().await.clone()
    }

    /// Persist current model health state to the settings DB.
    pub async fn persist_model_health_state(&self) {
        use djinn_db::SettingsRepository;
        const MODEL_HEALTH_STATE_KEY: &str = "model_health.state";
        let repo = SettingsRepository::new(self.db.clone(), self.event_bus.clone());
        let snapshot = self.health_tracker.all_health();
        match serde_json::to_string(&snapshot) {
            Ok(raw) => {
                if let Err(e) = repo.set(MODEL_HEALTH_STATE_KEY, &raw).await {
                    tracing::warn!(error = %e, "failed to persist model health state");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to serialize model health state"),
        }
    }
}
