//! The `SupervisorServices` trait — the object-safe surface the supervisor
//! orchestration loop calls into.
//!
//! See the crate docs for the PR 3 context. Two impls exist today:
//!
//! - `djinn_agent::direct_services::DirectServices` — in-process, wraps
//!   `AgentContext`. Production path.
//! - [`rpc::StubRpcServices`] — `unimplemented!()` placeholder that pins the
//!   trait layout ahead of PR 4/5.

use async_trait::async_trait;
use djinn_core::models::{Task, TaskRunStatus};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use crate::{RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec};

pub mod rpc;
pub mod server;
pub mod wire;

pub use wire::SerializableCreateTaskRunParams;

/// Dependencies shared across every stage in a task-run.
///
/// Object-safe by construction: no generic method parameters, no
/// `Self`-by-value receivers. `async_trait` handles the `Pin<Box<dyn
/// Future + Send>>` boxing so the trait can be used behind
/// `Arc<dyn SupervisorServices>`.
#[async_trait]
pub trait SupervisorServices: Send + Sync + 'static {
    /// Supervisor-wide cancellation token.  Flagged when the task-run is torn
    /// down (server shutdown, user kill).
    fn cancel(&self) -> &CancellationToken;

    /// Load the [`Task`] row backing this task-run.  Called once, before the
    /// first stage executes.
    async fn load_task(&self, task_id: String) -> Result<Task, String>;

    /// Execute one role stage against the shared workspace.  Called once per
    /// entry in `spec.flow.role_sequence()`.
    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError>;

    /// Open (or adopt) a GitHub PR for the completed task-run.  Called at
    /// most once per run, only for `NewTask` / `ReviewResponse` /
    /// `ConflictRetry` flows that reached the end of their role sequence
    /// cleanly.
    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome;

    /// Persist a new `task_run` row.  Phase 4 will switch
    /// [`crate::TaskRunSupervisor::run`] off its direct
    /// `Arc<TaskRunRepository>` and onto this RPC so the worker pod (which
    /// has no DB connection) can ship the same write through the existing
    /// `SupervisorServices` channel.  Dead code until Phase 4.
    async fn create_task_run(
        &self,
        params: SerializableCreateTaskRunParams,
    ) -> Result<(), String>;

    /// Update the terminal `status` (and `ended_at`) of a `task_run` row.
    /// Phase 4 will swap the supervisor's `task_runs.update_status` call
    /// for this method.  Dead code until Phase 4.
    async fn update_task_run_status(
        &self,
        run_id: String,
        status: TaskRunStatus,
    ) -> Result<(), String>;

    /// Look up a model's context window (in tokens) from the provider
    /// catalog by full `"providerID/modelID"` identifier.
    ///
    /// Phase 6b extraction — replaces direct
    /// `agent_context.catalog.find_model(..)` reads in
    /// `supervisor_impl::stage` and `direct_services`. Returns
    /// `Err("model not found")` when the catalog has no matching entry;
    /// callers may treat that as a soft fallback to `0`.
    async fn get_model_context_window(&self, model_id: String) -> Result<i64, String>;

    /// Look up a provider's `base_url` from the provider catalog by
    /// catalog provider id.
    ///
    /// Phase 6b extraction — replaces direct
    /// `agent_context.catalog.list_providers()...find(..)` reads in
    /// `supervisor_impl::stage` and `direct_services`. Returns
    /// `Err("provider not found")` (or `Err("provider has empty base_url")`)
    /// when the catalog has no matching entry; callers may treat that as a
    /// signal to fall back to `actors::slot::helpers::default_base_url`.
    async fn get_provider_base_url(
        &self,
        catalog_provider_id: String,
    ) -> Result<String, String>;

    /// Pick any available `"providerID/modelID"` from the catalog as a
    /// fallback default model.
    ///
    /// Phase 6b extraction — replaces the legacy `default_model_for_role`
    /// helper which walked `app_state.catalog.list_providers()` /
    /// `list_models(..)` directly. Returns `Ok(None)` when the catalog has
    /// no providers / no models.
    async fn pick_any_default_model(&self) -> Result<Option<String>, String>;
}
