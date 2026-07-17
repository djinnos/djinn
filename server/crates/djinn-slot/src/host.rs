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
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use djinn_control_plane::bridge::RuntimeOps;
use djinn_control_plane::tools::task_tools::ErrorResponse;
use djinn_core::clock::Clock;
use djinn_core::events::EventBus;
use djinn_core::models::Task;
use djinn_db::Database;
use djinn_orchestration_types::coordinator::BackgroundWorkTracker;
use djinn_orchestration_types::trigger::CoordinatorTrigger;
use djinn_provider::catalog::{CatalogService, HealthTracker};

use crate::final_verification::{
    FinalVerificationInvocationLease, FinalVerificationResolvedMaterial,
};
use crate::helpers::ProviderCredential;
use crate::reply_loop::compaction_guard::CompactionCriticalSection;

type FinalVerificationLeaseFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Box<dyn FinalVerificationInvocationLease>, String>> + Send + 'a>,
>;

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

/// Per-task last-activity timestamps (unix seconds).
pub type ActivityTracker = Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>;

/// Opaque host-specific operations that slot code invokes through callbacks.
///
/// The concrete implementation lives in `djinn-agent`. This trait avoids
/// `djinn-slot` depending on `djinn-agent` modules like `prompts`,
/// `mcp_client`, `task_merge`, `runtime_bridge`, `supervisor`, etc.
pub trait SlotHostCallbacks: Send + Sync + 'static {
    /// Resolve canonical material for an authoritative final-verification call.
    /// The default fails closed until a host wires completion intent.
    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        _verify_run_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<FinalVerificationResolvedMaterial, String>> + Send + 'a>>
    {
        Box::pin(async { Err("final verification resolution is not available".to_owned()) })
    }
    /// Acquire the ordinary per-invocation verification lease.
    fn acquire_final_verification_lease<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> FinalVerificationLeaseFuture<'a> {
        Box::pin(async { Err("final verification lease is not available".to_owned()) })
    }
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
    /// (including OAuth refresh when applicable). Serialization for worker
    /// dispatch is host-only and remains outside `djinn-slot`; this callback
    /// returns a live [`ProviderCredential`] for slot-side provider construction.
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
    ///
    /// The final `resume_lifecycle_metadata` argument is the additive
    /// resume-via-git lifecycle metadata selected at re-dispatch time. It
    /// rides the slot pipeline into the host's `dispatch_task_runtime`, which
    /// puts it on `TaskRunSpec::resume_lifecycle_metadata`. `None` means
    /// resume selection was not consulted (default/off path) and the host
    /// must preserve the legacy `None` on the spec.
    // The trait method is intentionally additive and the 8-arg signature
    // mirrors the existing 7-arg form plus the optional resume metadata
    // blob. Sibling tasks in the resume-via-git epic (twsk) tighten the
    // shape once the integration consumes it.
    #[allow(clippy::too_many_arguments)]
    fn run_task_dispatch<'a>(
        &'a self,
        task_id: String,
        project_path: String,
        model_id: String,
        ctx: SlotContext,
        kill: tokio_util::sync::CancellationToken,
        pause: tokio_util::sync::CancellationToken,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;
    /// Touch activity for stall detection (best-effort RPC to host's
    /// ActivityTracker).  On the host this is a local atomic write; on a
    /// worker it routes over RPC.  Fire-and-forget — transient errors are
    /// swallowed with a warn log because the local tracker still serves
    /// worker diagnostics.
    fn touch_activity_rpc<'a>(
        &'a self,
        task_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
    /// Flush session token counts to the session row (best-effort).
    /// Used by the streaming loop to persist mid-flight counters so
    /// long-running sessions don't read stale zeros until teardown.
    fn flush_session_tokens_rpc<'a>(
        &'a self,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

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

/// Trait for dispatching tool calls from the reply loop.
///
/// The host implements this trait to route tool calls through its stash,
/// MCP, and extension infrastructure.  The slot crate never imports
/// `djinn-agent` internals — all host-only tool behaviour is behind this
/// abstraction.
pub trait SlotToolDispatcher: Send + Sync + 'static {
    /// Check if a tool name is a stash-only tool (`output_view`, `output_grep`).
    fn is_stash_tool(&self, tool_name: &str) -> bool;
    /// Handle a stash tool call internally (`output_view`, `output_grep`).
    fn handle_stash_call(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, String>;
    /// Render a raw tool result through the host's truncation/stash pipeline.
    fn render_result(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        value: &serde_json::Value,
    ) -> String;
    /// Externalize an already-rendered tool result into a smaller inline stash
    /// stub with a caller-chosen preview size.
    ///
    /// This is the turn-budget externalization seam: the slot crate can ask the
    /// host to replace a previously-rendered result string with a compact
    /// `[djinn-output-stash ...]` stub (`reason="turn_budget"`) whose preview is
    /// `smart_truncate`d to at most `preview_chars` characters, while the full
    /// rendered text is preserved for `output_view` / `output_grep` recovery.
    ///
    /// The **host owns all stash mechanics** — `djinn-slot` never imports
    /// `djinn-agent` internals.  The host implementation decides whether to
    /// stash (it guards against non-shrinking replacements) and how to render
    /// the canonical header and recovery hint.
    ///
    /// Returns the externalized stub text, or the original `rendered` string
    /// unchanged when the host determines externalization would not reduce the
    /// inline size.
    fn externalize_rendered_result(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        rendered: &str,
        preview_chars: usize,
    ) -> String;
    /// Dispatch a tool call through the host's extension layer (non-stash,
    /// non-MCP tools).  The host resolves the tool, runs it, and returns
    /// the raw JSON result.
    fn dispatch_extension_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        worktree_path: &'a std::path::Path,
        task_id: &'a str,
        role_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = djinn_core::tool_call::ToolCallOutcome> + Send + 'a>>;
    /// Check if a tool name is registered as an MCP tool.
    fn is_mcp_tool(&self, tool_name: &str) -> bool;
    /// Dispatch an MCP tool call.
    fn dispatch_mcp_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'a>>;
    /// Return the MCP server name for a tool, if known.
    fn mcp_server_for_tool(&self, tool_name: &str) -> Option<String>;
    /// Check if a tool name is a native MCP resource tool
    /// (`list_mcp_resources` or `read_mcp_resource`).
    fn is_resource_tool(&self, tool_name: &str) -> bool;
    /// Dispatch a native MCP resource tool call.
    ///
    /// The host routes `list_mcp_resources` and `read_mcp_resource` through
    /// the MCP tool registry's resource methods.  Returns `Ok(json)` on
    /// success or `Err(message)` on failure.
    fn dispatch_resource_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
    /// Clear the session's stash (used during compaction resets).
    fn clear_stash(&self);
}

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
    /// Shared wall-clock seam. Production code constructs this with
    /// [`SystemClock::new`]; tests may inject a `TestClock` for deterministic
    /// time reads.  All wall-clock reads on slot runtime paths route through
    /// this field rather than calling `SystemTime::now()`/`Instant::now()`
    /// directly, so the `disallowed_methods` lint stays clean.
    pub clock: Arc<dyn Clock>,
    /// Host-specific callbacks for complex operations.
    pub callbacks: Arc<dyn SlotHostCallbacks>,
    /// Tool dispatch handle for the reply loop.  Routes stash, MCP, and
    /// extension tool calls through the host's infrastructure.
    pub tool_dispatcher: Option<Arc<dyn SlotToolDispatcher>>,
    /// Shared compaction critical section for the active slot lifecycle/run.
    ///
    /// The slot actor creates one per active lifecycle and passes it into the
    /// reply loop; it keeps a clone on `ActiveLifecycle` so command routing can
    /// observe/wait on compaction without busy waiting. This is internal to the
    /// slot lifecycle and is not serialized across the wire.
    pub compaction_cs: CompactionCriticalSection,
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
    /// Current unix-seconds timestamp read through the shared clock seam.
    /// Falls back to 0 when the wall-clock is before `UNIX_EPOCH`.
    fn now_unix_secs(&self) -> u64 {
        self.clock
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
    pub fn register_activity(&self, task_id: &str) -> Arc<AtomicU64> {
        let now = self.now_unix_secs();
        let ts = Arc::new(AtomicU64::new(now));
        recover_lock(&self.active_tasks, "active_tasks").insert(task_id.to_string(), ts.clone());
        ts
    }
    pub fn touch_activity(&self, task_id: &str) {
        let now = self.now_unix_secs();
        let mut guard = recover_lock(&self.active_tasks, "active_tasks");
        match guard.get(task_id) {
            Some(ts) => ts.store(now, Ordering::Relaxed),
            None => {
                guard.insert(task_id.to_string(), Arc::new(AtomicU64::new(now)));
            }
        }
    }
    pub fn deregister_activity(&self, task_id: &str) {
        recover_lock(&self.active_tasks, "active_tasks").remove(task_id);
    }
    pub fn idle_seconds(&self, task_id: &str) -> Option<u64> {
        let now = self.now_unix_secs();
        let guard = recover_lock(&self.active_tasks, "active_tasks");
        let ts = guard.get(task_id)?;
        let last = ts.load(Ordering::Relaxed);
        Some(now.saturating_sub(last))
    }
    pub fn register_background_work(&self, task_id: &str) {
        recover_lock(&self.background_work_tasks, "background_work_tasks")
            .insert(task_id.to_string());
    }
    pub fn deregister_background_work(&self, task_id: &str) {
        recover_lock(&self.background_work_tasks, "background_work_tasks").remove(task_id);
    }
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
    pub fn mcp_state(&self) -> djinn_control_plane::McpState {
        self.callbacks.build_mcp_state(self)
    }
    pub async fn load_task(&self, task_id: &str) -> Result<Task, String> {
        let task_repo = djinn_db::TaskRepository::new(self.db.clone(), self.event_bus.clone());
        task_repo
            .get(task_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("task {task_id} not found"))
    }
}

/// Acquire a `std::sync::Mutex` guard, recovering from poison with a warning.
///
/// Mutex poisoning only occurs when a previous holder panicked — a programming
/// invariant violation.  The guarded data remains structurally valid, so we log
/// the anomaly and continue operating rather than cascading the panic.
fn recover_lock<'a, T>(mutex: &'a Mutex<T>, label: &'static str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(
                mutex = label,
                "std::sync::Mutex poisoned by prior panic; recovering with data"
            );
            poisoned.into_inner()
        }
    }
}
