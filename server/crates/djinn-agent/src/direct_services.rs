//! `DirectServices` — in-process [`SupervisorServices`] impl.
//!
//! Phase 2 PR 3 replaced `djinn-supervisor`'s struct-with-callbacks
//! `SupervisorServices` with a trait.  `DirectServices` is the production
//! (and integration-test) impl: it wraps an [`AgentContext`], a
//! supervisor-wide [`CancellationToken`], and an optional test-only
//! [`LlmProvider`] override, and delegates every trait method straight into
//! the in-tree lifecycle helpers.  Behaviour is verbatim with PR 2 — this
//! file just reshapes the closure bodies that used to live on
//! `SupervisorServices` into trait-method bodies.
//!
//! The worker-side sibling impl (`djinn_supervisor::services::rpc::StubRpcServices`
//! → real RPC client in PR 4/5) lives on the other side of the crate split
//! so this code never links the bincode/Unix-socket plumbing.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_core::models::{Task, TaskRunStatus};
use djinn_db::TaskRunRepository;
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_supervisor::services::SerializableCreateTaskRunParams;
use djinn_supervisor::{
    RoleKind, StageError, StageOutcome, SupervisorServices, TaskRunOutcome, TaskRunSpec,
};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use djinn_provider::provider::LlmProvider;
use crate::supervisor_impl::{SupervisorCallbackContext, execute_stage, supervisor_pr_open};

/// In-process `SupervisorServices` impl that delegates straight to the
/// lifecycle helpers inside `djinn-agent`.
pub struct DirectServices {
    callbacks: SupervisorCallbackContext,
    /// Bound to the same `Database` carried in `callbacks.agent_context`.
    /// Phase 3 adds this so [`SupervisorServices::create_task_run`] and
    /// [`SupervisorServices::update_task_run_status`] can persist
    /// `task_run` rows in-process; until Phase 4 cuts the
    /// `TaskRunSupervisor::run` body over to the trait, these methods are
    /// dead code (the supervisor still calls `task_runs.create()` /
    /// `task_runs.update_status()` directly).
    task_runs: Arc<TaskRunRepository>,
}

impl DirectServices {
    /// Construct a `DirectServices` bound to the given [`AgentContext`] and
    /// cancellation token.  Production path.
    pub fn new(agent_context: AgentContext, cancel: CancellationToken) -> Self {
        Self::with_provider_override(agent_context, cancel, None)
    }

    /// Same as [`DirectServices::new`] but installs a test-only
    /// [`LlmProvider`] override on the stage executor, bypassing the catalog
    /// / vault credential lookup inside `execute_stage`.  Used by
    /// `tests/phase1_supervisor.rs`.
    pub fn with_provider_override(
        agent_context: AgentContext,
        cancel: CancellationToken,
        provider_override: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        let task_runs = Arc::new(TaskRunRepository::new(agent_context.db.clone()));
        Self {
            callbacks: SupervisorCallbackContext {
                agent_context,
                cancel,
                provider_override,
            },
            task_runs,
        }
    }
}

#[async_trait]
impl SupervisorServices for DirectServices {
    fn cancel(&self) -> &CancellationToken {
        &self.callbacks.cancel
    }

    async fn load_task(&self, task_id: String) -> Result<Task, String> {
        crate::actors::slot::helpers::load_task(&task_id, &self.callbacks.agent_context)
            .await
            .map_err(|e| e.to_string())
    }

    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        execute_stage(
            task,
            workspace,
            role_kind,
            task_run_id,
            spec,
            &self.callbacks,
            self,
        )
        .await
    }

    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome {
        supervisor_pr_open(spec, task, &self.callbacks).await
    }

    async fn create_task_run(
        &self,
        params: SerializableCreateTaskRunParams,
    ) -> Result<(), String> {
        self.task_runs
            .create(CreateTaskRunParams {
                id: params.id.as_str(),
                project_id: params.project_id.as_str(),
                task_id: params.task_id.as_str(),
                trigger_type: params.trigger_type.as_str(),
                status: params.status.as_deref(),
                workspace_path: params.workspace_path.as_deref(),
                mirror_ref: params.mirror_ref.as_deref(),
            })
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn update_task_run_status(
        &self,
        run_id: String,
        status: TaskRunStatus,
    ) -> Result<(), String> {
        self.task_runs
            .update_status(&run_id, status)
            .await
            .map_err(|e| e.to_string())
    }

    async fn get_model_context_window(&self, model_id: String) -> Result<i64, String> {
        self.callbacks
            .agent_context
            .catalog
            .find_model(&model_id)
            .map(|m| m.context_window)
            .ok_or_else(|| format!("model not found in catalog: {model_id}"))
    }

    async fn get_provider_base_url(
        &self,
        catalog_provider_id: String,
    ) -> Result<String, String> {
        let base_url = self
            .callbacks
            .agent_context
            .catalog
            .list_providers()
            .iter()
            .find(|p| p.id == catalog_provider_id)
            .map(|p| p.base_url.clone())
            .ok_or_else(|| format!("provider not found in catalog: {catalog_provider_id}"))?;
        if base_url.is_empty() {
            return Err(format!(
                "provider has empty base_url in catalog: {catalog_provider_id}"
            ));
        }
        Ok(base_url)
    }

    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        let catalog = &self.callbacks.agent_context.catalog;
        for provider in catalog.list_providers() {
            if let Some(model) = catalog.list_models(&provider.id).first() {
                return Ok(Some(format!("{}/{}", provider.id, model.id)));
            }
        }
        Ok(None)
    }
}
