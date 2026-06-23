//! Host integration seam for the djinn-slot crate.
//!
//! [`SlotContext`] is the concrete host context that slot code uses instead of
//! `djinn_agent::context::SlotContext`.  It carries the service handles slot
//! code needs and an opaque callback object for host-specific operations
//! (MCP resolution, prompt rendering, task merge, etc.).
//!
//! `djinn-agent` constructs a `SlotContext` from its `SlotContext` and passes
//! it into the slot pool / actor.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use djinn_control_plane::bridge::RuntimeOps;
use djinn_control_plane::tools::task_tools::ErrorResponse;
use djinn_core::events::EventBus;
use djinn_core::models::Task;
use djinn_db::Database;
use djinn_orchestration_types::coordinator::BackgroundWorkTracker;
use djinn_orchestration_types::trigger::CoordinatorTrigger;
use djinn_provider::catalog::{CatalogService, HealthTracker};

use crate::helpers::ProviderCredential;

// ─── KnowledgeBranchTarget ──────────────────────────────────────────────────

/// Identifies the knowledge-write target for a session.
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

// ─── ActivityTracker ────────────────────────────────────────────────────────

/// Per-task last-activity timestamps (unix seconds).
pub type ActivityTracker = Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>;

// ─── Host callback trait ────────────────────────────────────────────────────

/// Opaque host-specific operations that slot code invokes through callbacks.
///
/// The concrete implementation lives in `djinn-agent`. This trait avoids
/// `djinn-slot` depending on `djinn-agent` modules like `prompts`,
/// `mcp_client`, `task_merge`, `runtime_bridge`, `supervisor`, etc.
pub trait SlotHostCallbacks: Send + Sync + 'static {
    /// Interrupt a paused worker session.
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        task_id: &'a str,
        ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Build the MCP tool registry for a worktree+role combination.
    fn resolve_mcp_tools<'a>(
        &'a self,
        worktree_path: &'a str,
        role_name: &'a str,
        ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<ResolvedMcpTools, String>> + Send + 'a>>;

    /// Render the prompt for a role.
    fn render_prompt(
        &self,
        role_name: &str,
        task: &Task,
        context_json: &serde_json::Value,
    ) -> String;

    /// Build the initial user message for a task session.
    fn initial_user_message<'a>(
        &'a self,
        task_id: &'a str,
        ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>>;

    /// Build a control-plane McpState from the slot context.
    fn build_mcp_state(&self, ctx: &SlotContext) -> djinn_control_plane::McpState;

    /// Resolve a project path to a project ID through the control-plane.
    fn require_project_id_for_task_ops<'a>(
        &'a self,
        project: &'a str,
        ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ErrorResponse>> + Send + 'a>>;

    /// Resolve a provider credential from the host's credential store
    /// (including OAuth refresh when applicable).  Returns the credential
    /// as a serializable JSON blob; the slot crate uses
    /// [`ProviderCredential`] to carry it.
    fn resolve_provider_credential<'a>(
        &'a self,
        provider_id: &'a str,
        ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCredential, String>> + Send + 'a>>;

    /// Run the supervisor dispatch for a task.  This encapsulates the entire
    /// `supervisor_runner::run_supervisor_dispatch` logic and its dependencies
    /// (runtime_bridge, supervisor, lifecycle stages, reply_loop, etc.).
    ///
    /// The host provides this because it depends on many djinn-agent modules
    /// that have not yet been extracted.
    fn run_task_dispatch<'a>(
        &'a self,
        task_id: String,
        project_path: String,
        model_id: String,
        ctx: SlotContext,
        kill: tokio_util::sync::CancellationToken,
        pause: tokio_util::sync::CancellationToken,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
}

// ─── ResolvedMcpTools ───────────────────────────────────────────────────────

/// Resolved MCP tools for a slot session.
#[derive(Clone)]
pub struct ResolvedMcpTools {
    pub tool_definitions: Vec<serde_json::Value>,
    pub registry_handle: Arc<dyn ToolRegistryHandle>,
}

/// Opaque handle to a host-managed MCP tool registry.
pub trait ToolRegistryHandle: Send + Sync {
    fn dispatch_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'a>>;
}

// ─── SlotContext ────────────────────────────────────────────────────────────

/// Concrete host context for slot operations.
///
/// This is the slot crate's equivalent of `SlotContext`.  It carries all
/// service handles that slot code needs and delegates complex host operations
/// to the [`SlotHostCallbacks`] trait object.
#[derive(Clone)]
pub struct SlotContext {
    pub db: Database,
    pub event_bus: EventBus,
    pub catalog: CatalogService,
    pub health_tracker: HealthTracker,
    pub background_work_tasks: BackgroundWorkTracker,
    pub active_tasks: ActivityTracker,
    pub default_project_id: Option<String>,
    pub working_root: Option<PathBuf>,
    /// Best-effort coordinator dispatch trigger.
    pub coordinator_trigger: Option<Arc<dyn CoordinatorTrigger>>,
    /// The runtime-ops handle for task-run management.
    pub runtime_ops: Option<Arc<dyn RuntimeOps>>,
    /// Code-graph operations handle (for auto code-context features).
    pub repo_graph_ops: Option<Arc<dyn djinn_control_plane::bridge::RepoGraphOps>>,
    /// Host-specific callbacks for complex operations.
    pub callbacks: Arc<dyn SlotHostCallbacks>,
}

impl SlotContext {
    /// Returns the working root for code-reading tools.
    pub fn working_root_for(&self, fallback: &Path) -> PathBuf {
        match self.working_root.as_deref() {
            Some(p) => p.to_path_buf(),
            None => fallback.to_path_buf(),
        }
    }

    pub fn default_project_id(&self) -> Option<&str> {
        self.default_project_id.as_deref()
    }

    /// Resolve the knowledge-write target for a session.
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

    // ── Activity tracking ──────────────────────────────────────────────

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

    pub fn touch_activity(&self, task_id: &str) {
        let now = SystemTime::now()
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

    pub fn deregister_activity(&self, task_id: &str) {
        self.active_tasks.lock().expect("poisoned").remove(task_id);
    }

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

    // ── Background work ────────────────────────────────────────────────

    pub fn register_background_work(&self, task_id: &str) {
        self.background_work_tasks
            .lock()
            .expect("poisoned")
            .insert(task_id.to_string());
    }

    pub fn deregister_background_work(&self, task_id: &str) {
        self.background_work_tasks
            .lock()
            .expect("poisoned")
            .remove(task_id);
    }

    // ── Coordinator ────────────────────────────────────────────────────

    pub async fn trigger_dispatch_for_project(&self, _project_id: &str) {
        if let Some(ref trigger) = self.coordinator_trigger {
            trigger.try_trigger_dispatch();
        }
    }

    pub fn try_trigger_dispatch(&self) {
        if let Some(ref trigger) = self.coordinator_trigger {
            trigger.try_trigger_dispatch();
        }
    }

    // ── MCP state ──────────────────────────────────────────────────────

    pub fn mcp_state(&self) -> djinn_control_plane::McpState {
        self.callbacks.build_mcp_state(self)
    }

    // ── Task lookup ────────────────────────────────────────────────────

    pub async fn load_task(&self, task_id: &str) -> Result<Task, String> {
        let task_repo = djinn_db::TaskRepository::new(self.db.clone(), self.event_bus.clone());
        task_repo
            .get(task_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("task {task_id} not found"))
    }
}
