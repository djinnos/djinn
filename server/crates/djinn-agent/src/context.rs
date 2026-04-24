use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use djinn_git::{GitActorHandle, GitError};
use djinn_runtime::GraphWarmerService;
use djinn_workspace::MirrorManager;
use djinn_control_plane::{
    McpState, bridge,
    bridge::{
        ApiSurfaceEntry, BoundaryRule, BoundaryViolation, ChangedRange, CycleGroup,
        DeadSymbolEntry, DeprecatedHit, DiffTouchesResult, EdgeEntry, HotPathHit, HotspotEntry,
        ImpactResult, MetricsAtResult, NeighborsResult, OrphanEntry, PathResult, ProjectCtx,
        RankedNode, RepoGraphOps, SearchHit, SymbolAtHit, SymbolDescription,
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
    /// Optional hook into the server's canonical-graph warming pipeline.
    /// When `Some`, the architect lifecycle calls
    /// [`GraphWarmerService::await_fresh`] before starting a session and the
    /// coordinator tick loop calls [`GraphWarmerService::trigger`] on a
    /// 10-minute cadence (see ADR-051 §3).  When `None` (tests, off-server
    /// contexts) both paths are skipped.
    pub graph_warmer: Option<Arc<dyn GraphWarmerService>>,
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
    /// Shared bare-mirror manager. Used by the mirror-native merge path in
    /// `task_merge` to run squash-merges against an ephemeral hardlinked
    /// clone instead of a worktree under `.djinn/worktrees/.merge-*`.
    ///
    /// `None` in test contexts that do not exercise the merge path — those
    /// contexts never hit `squash_merge_via_mirror`, which bails out with a
    /// clear error when the field is absent.
    pub mirror: Option<Arc<MirrorManager>>,
    /// Process-wide [`djinn_supervisor::ConnectionRegistry`].  Wired at the
    /// server boundary into every `AgentContext` produced by
    /// `AppState::agent_context()` so the slot runner (Phase 2.1 Phase E)
    /// can hand it to `KubernetesRuntime::new` — the runtime and the
    /// launcher's TCP listener share a single registry that way.  `None`
    /// in test contexts that never exercise the K8s runtime.
    pub rpc_registry: Option<Arc<djinn_supervisor::ConnectionRegistry>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeBranchTarget {
    Main,
    TaskScoped { worktree_root: PathBuf },
}

impl KnowledgeBranchTarget {
    pub fn worktree_root(&self) -> Option<&Path> {
        match self {
            Self::Main => None,
            Self::TaskScoped { worktree_root } => Some(worktree_root.as_path()),
        }
    }

    pub fn intent_label(&self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::TaskScoped { .. } => "task",
        }
    }
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

    /// Resolve the knowledge-write target for a session.
    ///
    /// Task runs with a preserved workspace route note writes into that
    /// task-scoped tree so extracted notes participate in the same promotion /
    /// discard lifecycle as agent-authored memory mutations. Runs without a
    /// usable workspace fall back to canonical-main writes, preserving the
    /// default SQLite-backed behavior for contexts that do not opt into
    /// task-branch routing.
    ///
    /// The `workspace_path` is now sourced from `task_runs.workspace_path`
    /// (via `sessions.task_run_id`) rather than `sessions.worktree_path`.
    pub fn knowledge_branch_target_for(
        &self,
        project_root: &Path,
        workspace_path: Option<&str>,
    ) -> KnowledgeBranchTarget {
        let Some(workspace_path) = workspace_path
            .map(str::trim)
            .filter(|path| !path.is_empty())
        else {
            return KnowledgeBranchTarget::Main;
        };

        let worktree_root = PathBuf::from(workspace_path);
        if worktree_root == project_root {
            KnowledgeBranchTarget::Main
        } else {
            KnowledgeBranchTarget::TaskScoped { worktree_root }
        }
    }

    pub fn knowledge_worktree_root_for(
        &self,
        project_root: &Path,
        workspace_path: Option<&str>,
    ) -> Option<PathBuf> {
        self.knowledge_branch_target_for(project_root, workspace_path)
            .worktree_root()
            .map(Path::to_path_buf)
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

    async fn embed_memory_query(
        &self,
        _: &str,
    ) -> Result<Option<bridge::SemanticQueryEmbedding>, String> {
        Ok(None)
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

    async fn apply_environment_config(
        &self,
        _project_id: &str,
        _config: &djinn_stack::environment::EnvironmentConfig,
    ) -> Result<(), String> {
        // The agent-internal runtime doesn't own the kube client /
        // ImageController — only the server-side AppState impl does.
        // If this path ever fires from in-agent MCP invocation, surface
        // a clear error so the caller knows they're wired up wrong.
        Err("apply_environment_config is unavailable on the agent-internal runtime — \
             route project_environment_config_set through djinn-server's MCP endpoint"
            .into())
    }
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
        _: &ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn ranked(
        &self,
        _: &ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<RankedNode>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn implementations(&self, _: &ProjectCtx, _: &str) -> Result<Vec<String>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn impact(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: usize,
        _: Option<&str>,
    ) -> Result<ImpactResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn search(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<SearchHit>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn cycles(
        &self,
        _: &ProjectCtx,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<CycleGroup>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn orphans(
        &self,
        _: &ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn path(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: &str,
        _: Option<usize>,
    ) -> Result<Option<PathResult>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn edges(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<EdgeEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn describe(
        &self,
        _: &ProjectCtx,
        _: &str,
    ) -> Result<Option<SymbolDescription>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn status(&self, _: &ProjectCtx) -> Result<bridge::GraphStatus, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn symbols_at(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: u32,
        _: Option<u32>,
    ) -> Result<Vec<SymbolAtHit>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn diff_touches(
        &self,
        _: &ProjectCtx,
        _: &[ChangedRange],
    ) -> Result<DiffTouchesResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn api_surface(
        &self,
        _: &ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<ApiSurfaceEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn boundary_check(
        &self,
        _: &ProjectCtx,
        _: &[BoundaryRule],
    ) -> Result<Vec<BoundaryViolation>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn hotspots(
        &self,
        _: &ProjectCtx,
        _: u32,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<HotspotEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn metrics_at(&self, _: &ProjectCtx) -> Result<MetricsAtResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn dead_symbols(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn deprecated_callers(
        &self,
        _: &ProjectCtx,
        _: usize,
    ) -> Result<Vec<DeprecatedHit>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn touches_hot_path(
        &self,
        _: &ProjectCtx,
        _: &[String],
        _: &[String],
        _: &[String],
    ) -> Result<Vec<HotPathHit>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn coupling(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<bridge::CouplingEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn churn(
        &self,
        _: &ProjectCtx,
        _: usize,
        _: Option<u32>,
    ) -> Result<Vec<bridge::ChurnEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn coupling_hotspots(
        &self,
        _: &ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<bridge::CoupledPairEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn coupling_hubs(
        &self,
        _: &ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<bridge::CouplingHubEntry>, String> {
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
    /// Build the public djinn-control-plane state bridge expected by the shared task-mutation ops.
    ///
    /// This keeps djinn-agent on the supported ADR-041 seam: shared business logic stays in
    /// djinn-control-plane, while agent adapters reuse their existing database/event-bus/project resolver
    /// dependencies instead of reconstructing MCP internals locally.
    pub fn to_mcp_state(&self) -> McpState {
        McpState::new(
            self.db.clone(),
            self.event_bus.clone(),
            self.catalog.clone(),
            self.health_tracker.clone(),
            None,
            None,
            None,
            None,
            Arc::new(self.lsp.clone()),
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

    /// Resolve a project path through the same public djinn-control-plane contract used by external
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
        let server = djinn_control_plane::server::DjinnMcpServer::new(self.to_mcp_state());
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
                // Fall back to reverse-parsing the `{projects_root}/{owner}/{repo}`
                // clone path shape. The `Project` identity is now `(github_owner,
                // github_repo)`; any raw filesystem path we get here is expected
                // to end in that two-segment tail.
                let repo =
                    djinn_db::ProjectRepository::new(self.db.clone(), self.event_bus.clone());
                let owner_repo = std::path::Path::new(project)
                    .components()
                    .rev()
                    .take(2)
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                if owner_repo.len() < 2 {
                    return Err(error);
                }
                // rev().take(2) yields [repo, owner]; flip them.
                let repo_name = &owner_repo[0];
                let owner_name = &owner_repo[1];
                repo.get_by_github(owner_name, repo_name)
                    .await
                    .map_err(|repo_error| ErrorResponse::new(repo_error.to_string()))?
                    .map(|p| p.id)
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
