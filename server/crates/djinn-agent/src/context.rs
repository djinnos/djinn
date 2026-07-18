use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
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
use djinn_git::{GitActorHandle, GitError};
use djinn_runtime::GraphWarmerService;
use djinn_workspace::MirrorManager;
use tokio::sync::Mutex;

use crate::actors::coordinator::CoordinatorHandle;
use crate::file_time::FileTime;
use crate::lsp::LspManager;
use crate::roles::RoleRegistry;
use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};
use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_orchestration_types::coordinator::BackgroundWorkTracker;
use djinn_provider::catalog::{CatalogService, HealthTracker};
use djinn_slot::reply_loop::CompactionCriticalSection;

/// Shared tracker for per-task last-activity timestamps (unix seconds).
/// Used by stall detection to kill sessions that stop producing tokens.
pub type ActivityTracker = Arc<std::sync::Mutex<HashMap<String, Arc<AtomicU64>>>>;

/// Configuration injected into the optional session-start memory intent planner.
///
/// This is deliberately not environment-backed: callers and tests inject an
/// explicit value, keeping the default hot path disabled before prompt rendering
/// or any planner invocation. Provider wiring consumes this configuration in a
/// later lifecycle integration step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIntentPlannerConfig {
    /// Enables the planner. The safe default is disabled.
    pub enabled: bool,
    /// Upper bound for a single planner call.
    pub timeout: Duration,
    /// Upper bound for provider completion output. The provider adapter selects
    /// its concrete unit while this boundary carries the requested limit.
    pub max_output: usize,
    /// Explicit cheap-model override; `None` delegates model choice to the
    /// future provider integration.
    pub model: Option<String>,
}

impl Default for MemoryIntentPlannerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: Duration::from_secs(5),
            max_output: 2_048,
            model: None,
        }
    }
}

impl MemoryIntentPlannerConfig {
    /// Build deployment configuration while preserving the safe disabled default.
    pub fn from_env() -> Self {
        let enabled = std::env::var("DJINN_MEMORY_INTENT_PLANNER_ENABLED")
            .map(|value| parse_bool_env(&value))
            .unwrap_or(false);
        Self {
            enabled,
            ..Self::default()
        }
    }

    /// Gate to evaluate before rendering the prompt or invoking a planner.
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Configuration for the periodic reconciliation sweep that reaps stale PRs,
/// branches, and orphan worker sessions.
///
/// Read from environment variables at construction time.  Defaults are
/// conservative: the sweep is **disabled** and in **dry-run** mode so that
/// first runs are safe.  Operators opt in by setting
/// `DJINN_RECONCILIATION_SWEEP_ENABLED=true`.
#[derive(Debug, Clone)]
pub struct ReconciliationSweepConfig {
    /// Whether the reconciliation sweep is active.
    /// Env: `DJINN_RECONCILIATION_SWEEP_ENABLED` (default `false`).
    pub enabled: bool,
    /// When `true`, the sweep logs what it *would* do without calling GitHub.
    /// Env: `DJINN_RECONCILIATION_SWEEP_DRY_RUN` (default `true`).
    pub dry_run: bool,
    /// Seconds after a task closes before its PR/branch becomes eligible for
    /// sweep reaping.
    /// Env: `DJINN_RECONCILIATION_SWEEP_GRACE_PERIOD_SECS` (default `600`).
    pub grace_period: Duration,
}

impl Default for ReconciliationSweepConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            grace_period: Duration::from_secs(600),
        }
    }
}

impl ReconciliationSweepConfig {
    /// Build a config from environment variables, falling back to safe defaults.
    pub fn from_env() -> Self {
        let enabled = std::env::var("DJINN_RECONCILIATION_SWEEP_ENABLED")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(false);

        let dry_run = std::env::var("DJINN_RECONCILIATION_SWEEP_DRY_RUN")
            .map(|v| parse_bool_env(&v))
            .unwrap_or(true);

        let grace_period_secs = std::env::var("DJINN_RECONCILIATION_SWEEP_GRACE_PERIOD_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600);

        Self {
            enabled,
            dry_run,
            grace_period: Duration::from_secs(grace_period_secs),
        }
    }
}

/// Parse a boolean environment variable value.  Accepts `1`, `true`, `yes`
/// (case-insensitive) as truthy; everything else is falsy.
fn parse_bool_env(val: &str) -> bool {
    matches!(
        val.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
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
    /// Tasks with in-flight post-session background work (merge/transition for
    /// non-worker roles, knowledge extraction). The coordinator's stuck-task
    /// recovery skips releasing a task while it is registered here, so a slot
    /// that freed its session but is still finishing background work isn't
    /// spuriously recovered.
    pub background_work_tasks: BackgroundWorkTracker,
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
    /// `.task-runtime/worktrees/_index/` checkout pinned to `origin/main`.  Workers,
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
    /// Real `RuntimeOps` implementation injected at the server boundary
    /// (typically `AppState`, via the server crate's bridge impl). Slot-pool
    /// interrupt paths use this to route task-run Job teardown through the
    /// control-plane seam without depending on Kubernetes directly. `None` in
    /// off-server/test contexts falls back to the agent-internal no-op runtime.
    pub runtime_ops: Option<Arc<dyn bridge::RuntimeOps>>,
    /// Root under which coordinator stale-resource GC enumerates per-task-run
    /// Cargo target directories. `None` uses the production PVC path
    /// (`/cache/cargo-target-runs`); tests may inject a temp dir so sweeps never
    /// touch the shared build cache used by the test process itself.
    pub cargo_target_runs_root: Option<PathBuf>,
    /// Shared bare-mirror manager. Used by the mirror-native merge path in
    /// `task_merge` to run squash-merges against an ephemeral hardlinked
    /// clone instead of a worktree under `.task-runtime/worktrees/.merge-*`.
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
    /// Default project_id for tool calls that don't pass `project` explicitly.
    /// Set on the K8s worker side from `TaskRunSpec.project_id` so the worker —
    /// which only ever serves one project per Pod — doesn't need the LLM to
    /// remember to pass a `project` arg to every multi-project-aware tool
    /// (epic_show, epic_tasks, task_*, etc.). Without this, tools fall through
    /// to "project is required when multiple projects are configured" and
    /// burn LLM tokens on retries. `None` in host-side AgentContexts (chat
    /// surface + multi-project planners) where the caller IS expected to
    /// disambiguate.
    pub default_project_id: Option<String>,
    /// Immutable cross-project shell authority copied from the task-run spec.
    /// An untrusted child process cannot grant itself another source through
    /// environment configuration.
    pub read_source_authorization: ReadSourceAuthorization,
    /// Runtime-owned configuration for the optional session-start memory
    /// planner. Composition roots may inject an enabled value; ordinary
    /// construction keeps the feature disabled.
    pub memory_intent_planner: MemoryIntentPlannerConfig,
    /// Configuration for the periodic reconciliation sweep (stale PRs,
    /// branches, and orphan worker sessions).  Read from environment
    /// variables at `AgentContext` construction time via
    /// [`ReconciliationSweepConfig::from_env`].
    pub reconciliation_sweep: ReconciliationSweepConfig,
    /// Explicitly injected configuration for the optional session-start memory
    /// intent planner. It remains disabled by default, but keeping it on the
    /// dispatch context makes an enabled deployment reachable without a local
    /// stage-level replacement of the configured value.
    /// Shared compaction critical section for the active slot lifecycle/run.
    ///
    /// This is normally owned by the slot actor and threaded into the reply
    /// loop via `SlotContext`. `AgentContext` carries a clone for the legacy
    /// agent-side helper that builds `SlotContext` from `AgentContext`.
    pub compaction_cs: CompactionCriticalSection,
}

/// Cross-project shell authority for the single task run represented by an
/// in-pod [`AgentContext`]. Host contexts use the empty default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadSourceAuthorization {
    pub owner_project_id: Option<String>,
    pub read_source_project_ids: Vec<String>,
    /// Host-provided root containing this owner's Release N cache. An absent
    /// root is a fail-closed missing-mount signal; workers never clone it.
    pub owner_cache_root: Option<PathBuf>,
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
        Err(
            "apply_environment_config is unavailable on the agent-internal runtime — \
             route project_environment_config_set through djinn-server's MCP endpoint"
                .into(),
        )
    }

    async fn trigger_mirror_refresh(&self, _project_id: &str) {
        // The agent-internal runtime doesn't own the mirror manager — only
        // the server-side AppState impl does. No-op here; the server's
        // periodic mirror-fetch tick still covers this project.
    }
    async fn enqueue_image_build(&self, _image_id: &str) -> Result<(), String> {
        // Catalog image builds are owned by the server-side AppState impl
        // (it owns the ImageController + kube client). No-op for the
        // agent-internal runtime; the server's reconcile tick covers it.
        Ok(())
    }
    async fn trigger_graph_warm(&self, _project_id: &str) {
        // Graph warming is owned by the server-side AppState impl (it owns
        // the K8s graph warmer). No-op for the agent-internal runtime.
    }
    async fn apply_user_model_change(&self) {
        // Slot-pool/coordinator reconfiguration is owned by the server-side
        // AppState impl. No-op for the agent-internal runtime.
    }
    async fn teardown_taskrun_job(&self, _task_run_id: &str) -> Result<(), String> {
        // Task-run Job deletion is owned by the server-side AppState impl via
        // the K8s graph warmer. Agent-internal/test contexts have no kube
        // client, so this is an idempotent no-op stub.
        Ok(())
    }
    async fn list_taskrun_jobs(
        &self,
    ) -> Result<Vec<djinn_control_plane::bridge::TaskrunJobRef>, String> {
        // Agent-internal/test contexts have no kube client. Return a stable
        // empty inventory so coordinator tests can exercise DB-side decisions
        // without importing djinn-k8s.
        Ok(Vec::new())
    }
    async fn cleanup_task_branches(&self, _task_id: &str) {
        // Branch/PR cleanup needs the mirror manager, which the agent-internal
        // runtime doesn't own — only the server-side AppState impl does. The
        // abort cascade is only invoked server-side, so this is a no-op here.
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
        _: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn ranked(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
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
        _workspace: Option<&str>,
        _: &str,
        _: usize,
        _: Option<&str>,
        _: Option<f64>,
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
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn path(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
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
    async fn describe(&self, _: &ProjectCtx, _: &str) -> Result<Option<SymbolDescription>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn context(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: bool,
    ) -> Result<Option<bridge::SymbolContext>, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn status(&self, _: &ProjectCtx) -> Result<bridge::GraphStatus, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn snapshot(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
        _: djinn_control_plane::bridge::SnapshotLevel,
        _: usize,
        _: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<bridge::SnapshotPayload, String> {
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
    async fn detect_changes(
        &self,
        _: &ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: &[String],
    ) -> Result<bridge::DetectedChangesResult, String> {
        Err("code_graph not available in agent bridge — use MCP server".into())
    }
    async fn api_surface(
        &self,
        _: &ProjectCtx,
        _workspace: Option<&str>,
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
        _: &str,
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
    async fn complexity(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<bridge::ComplexityResult, String> {
        Err("complexity not available in agent bridge — use MCP server".into())
    }
    async fn refactor_candidates(
        &self,
        _: &ProjectCtx,
        _: Option<u32>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<bridge::RefactorCandidate>, String> {
        Err("refactor_candidates not available in agent bridge — use MCP server".into())
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
        _workspace: Option<&str>,
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
    async fn resolve(
        &self,
        _: &ProjectCtx,
        _: &str,
        _: Option<&str>,
    ) -> Result<bridge::ResolveOutcome, String> {
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

struct LspOpsAdapter(pub LspManager);

#[async_trait]
impl bridge::LspOps for LspOpsAdapter {
    async fn warnings(&self) -> Vec<bridge::LspWarning> {
        self.0
            .warnings()
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
        let runtime_ops = self.runtime_ops.clone().unwrap_or_else(|| {
            Arc::new(AgentRuntimeOps {
                db: self.db.clone(),
                event_bus: self.event_bus.clone(),
                health_tracker: self.health_tracker.clone(),
            }) as Arc<dyn bridge::RuntimeOps>
        });
        McpState::new(
            self.db.clone(),
            self.event_bus.clone(),
            self.catalog.clone(),
            self.health_tracker.clone(),
            None,
            None,
            None,
            None,
            Arc::new(LspOpsAdapter(self.lsp.clone())),
            runtime_ops,
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
                match repo
                    .get_by_github(owner_name, repo_name)
                    .await
                    .map_err(|repo_error| ErrorResponse::new(repo_error.to_string()))?
                    .map(|p| p.id)
                {
                    Some(id) => Ok(id),
                    None => {
                        // K8s worker fallback: the worktree path
                        // (/workspace/.tmpXXX) won't reverse-parse into
                        // owner/repo, but the pod is single-project so the
                        // spec.project_id default applies.
                        if let Some(default_id) =
                            self.default_project_id.as_deref().filter(|s| !s.is_empty())
                        {
                            return Ok(default_id.to_string());
                        }
                        Err(error)
                    }
                }
            }
        }
    }

    /// Get or spawn a `GitActorHandle` for the given project path.
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.git_actors.lock().await;
        djinn_git::get_or_spawn(&mut map, path)
    }

    /// Register a task as having in-flight post-session background work.
    pub fn register_background_work(&self, task_id: &str) {
        self.background_work_tasks
            .lock()
            .expect("poisoned")
            .insert(task_id.to_string());
    }

    /// Deregister a task's post-session background work (completed or crashed).
    pub fn deregister_background_work(&self, task_id: &str) {
        self.background_work_tasks
            .lock()
            .expect("poisoned")
            .remove(task_id);
    }

    /// Register a task as active and return the shared timestamp atomic.
    /// The atomic is initialized to the current unix timestamp.
    pub fn register_activity(&self, task_id: &str) -> Arc<AtomicU64> {
        let now = SystemClockTrait::new()
            .now()
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

    /// Update the activity timestamp for a task to now, **creating** the entry
    /// if it does not exist yet (upsert).
    ///
    /// The upsert is load-bearing for remote (K8s) workers. A worker runs its
    /// `reply_loop` — and therefore `register_activity` — inside its OWN process
    /// against its OWN `AgentContext`; the host never registers the worker's
    /// task. The worker bridges liveness to the host by calling the
    /// `touch_activity` RPC on every stream event / tool call. If this method
    /// only updated pre-existing entries, that RPC would be a no-op on the host,
    /// `idle_seconds` would always return `None`, and the coordinator's stall
    /// poller would fall back to wall-clock-since-`started_at` — killing every
    /// productive worker at the zero-token threshold mid-flow. Upserting makes
    /// the first touch register the task, so the host tracks true idle from the
    /// worker's first sign of life. The presence of an entry also doubles as the
    /// "this session has shown activity (is past its first LLM call)" signal the
    /// stall poller uses to grant the full role budget instead of the aggressive
    /// first-call cap.
    pub fn touch_activity(&self, task_id: &str) {
        let now = SystemClockTrait::new()
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut guard = self.active_tasks.lock().expect("poisoned");
        match guard.get(task_id) {
            Some(ts) => ts.store(now, Ordering::Relaxed),
            None => {
                guard.insert(task_id.to_string(), Arc::new(AtomicU64::new(now)));
            }
        }
    }

    /// Return seconds since last activity touch, or `None` if not registered.
    pub fn idle_seconds(&self, task_id: &str) -> Option<u64> {
        let now = SystemClockTrait::new()
            .now()
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

// ---------------------------------------------------------------------------
// ExtensionContext implementation
// ---------------------------------------------------------------------------
//
// Bridges the narrow `djinn_mcp_extension::ExtensionContext` capability seam
// to the concrete `AgentContext` fields.  Extension handlers operate through
// the trait; this impl is the only place that names both sides.

#[async_trait::async_trait]
impl djinn_mcp_extension::ExtensionContext for AgentContext {
    fn db(&self) -> Database {
        self.db.clone()
    }

    fn event_bus(&self) -> djinn_core::events::EventBus {
        self.event_bus.clone()
    }

    fn mcp_state(&self) -> djinn_control_plane::McpState {
        self.to_mcp_state()
    }

    fn lsp(&self) -> crate::lsp::LspManager {
        self.lsp.clone()
    }

    fn working_root_for(&self, fallback: &std::path::Path) -> std::path::PathBuf {
        AgentContext::working_root_for(self, fallback)
    }

    fn default_project_id(&self) -> Option<&str> {
        self.default_project_id.as_deref()
    }
}
