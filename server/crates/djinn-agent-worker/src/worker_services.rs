//! `WorkerSupervisorServices` — the in-Pod [`SupervisorServices`] impl that
//! drives the real [`djinn_supervisor::TaskRunSupervisor`] inside each
//! per-task-run worker Pod.
//!
//! Phase 7b of `~/.claude/plans/phase2-worker-execution-architecture.md`.
//!
//! ## Shape
//!
//! Every host-bound trait method (DB writes, SSE publish, catalog reads, PR
//! open, …) delegates straight to the [`RpcServices`] connection the worker
//! already holds open to djinn-server. `execute_stage` is the load-bearing
//! deviation: it runs LOCALLY by:
//!
//! 1. Constructing an [`LlmProvider`] worker-side from the per-role
//!    [`SerializableCredential`] mounted via the K8s Secret (Phase 7a).
//! 2. Invoking [`djinn_agent::supervisor::worker_execute_stage`] with that
//!    provider injected via `provider_override`, so the in-tree per-stage
//!    executor skips its catalog/vault path entirely.
//!
//! `invoke_llm` is forwarded to RPC for parity but the worker never calls it
//! in the supervisor drive — the host-side `DirectServices` keeps the
//! method live for chat-tool invocations.
//!
//! ## AgentContext caveat
//!
//! `supervisor_impl::stage::execute_stage` still threads an `AgentContext`
//! through every helper it calls. The worker builds an `AgentContext` whose
//! `db` connects to the test Dolt for integration coverage; production-Pod
//! behaviour for the DB-touching helpers
//! (`resolve_role_overrides`, `build_prompt_context`,
//! `spawn_post_session_work`, `task_merge::resolve_project_path_for_id`) is
//! a Phase 7-followup. See the integration test in
//! `tests/in_pod_drive.rs` for what currently surfaces.

use std::sync::Arc;

use async_trait::async_trait;
use djinn_agent::actors::slot::helpers::{
    OAuthConfigWire, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, parse_model_id,
};
use djinn_agent::context::AgentContext;
use djinn_agent::supervisor::worker_execute_stage;
use djinn_core::models::{SessionRecord, SessionStatus, Task, TaskRunStatus};
use djinn_provider::message::Conversation;
use djinn_provider::provider::{
    LlmProvider, LlmResponse, ProviderCapabilities, ProviderConfig, ToolChoice, create_provider,
};
use djinn_runtime::{ResolvedCredentials, RoleKind, SerializableCredential};
use djinn_stack::environment::EnvironmentConfig;
use djinn_supervisor::services::{
    SerializableCreateSessionParams, SerializableCreateTaskRunParams,
};
use djinn_supervisor::{
    RpcServices, StageError, StageOutcome, SupervisorServices, TaskRunOutcome, TaskRunSpec,
};
use djinn_workspace::Workspace;
use tokio_util::sync::CancellationToken;

/// In-Pod `SupervisorServices` implementation used by `djinn-agent-worker`.
pub struct WorkerSupervisorServices {
    rpc: Arc<RpcServices>,
    credentials: ResolvedCredentials,
    cancel: CancellationToken,
    agent_context: AgentContext,
}

impl WorkerSupervisorServices {
    /// Wire a worker-side services impl around the RPC connection,
    /// resolved credentials bundle, supervisor cancel token, and the panic-
    /// stub-ish `AgentContext` the in-Pod supervisor threads through the
    /// per-stage executor.
    pub fn new(
        rpc: Arc<RpcServices>,
        credentials: ResolvedCredentials,
        cancel: CancellationToken,
        agent_context: AgentContext,
    ) -> Self {
        Self {
            rpc,
            credentials,
            cancel,
            agent_context,
        }
    }
}

/// Reconstruct an [`LlmProvider`] from a per-role [`SerializableCredential`]
/// and the model identifier resolved for that role.
///
/// API-key credentials mirror the host-side construction in
/// `djinn_agent::supervisor_impl::stage` — same auth method / format family /
/// capability defaults — minus telemetry (host owns Langfuse) and minus a
/// session-affinity key (the worker has no session_id yet at construction).
/// OAuth credentials deserialise the opaque JSON blob into
/// [`OAuthConfigWire`] and back into a live [`ProviderConfig`].
pub(crate) fn build_provider_from_serializable(
    cred: &SerializableCredential,
    model_id: &str,
    context_window: u32,
) -> Result<Arc<dyn LlmProvider>, StageError> {
    match cred {
        SerializableCredential::ApiKey { api_key, .. } => {
            let (provider_id, model_name) = parse_model_id(model_id)
                .map_err(|e| StageError::ModelResolution(format!("parse_model_id: {e}")))?;
            let format_family = format_family_for_provider(&provider_id, &model_name);
            let base_url = default_base_url(&provider_id);
            let provider = create_provider(ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&provider_id, api_key),
                format_family,
                model_id: model_name,
                context_window,
                telemetry: None,
                session_affinity_key: None,
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&provider_id),
            });
            Ok(Arc::from(provider))
        }
        SerializableCredential::OAuthConfig { config_json } => {
            let wire: OAuthConfigWire = serde_json::from_str(config_json).map_err(|e| {
                StageError::ModelResolution(format!("deserialize OAuth ProviderConfig: {e}"))
            })?;
            let (_, model_name) = parse_model_id(model_id)
                .map_err(|e| StageError::ModelResolution(format!("parse_model_id: {e}")))?;
            let mut cfg = wire.to_provider_config();
            cfg.model_id = model_name;
            cfg.context_window = context_window;
            cfg.telemetry = None;
            cfg.session_affinity_key = None;
            // `capabilities` survives the round-trip via OAuthCapabilitiesWire
            // but a defensive default keeps `streaming` truthy if the host
            // shipped a zero-value blob.
            if !cfg.capabilities.streaming {
                cfg.capabilities = ProviderCapabilities {
                    streaming: true,
                    max_tokens_default: cfg.capabilities.max_tokens_default,
                };
            }
            let provider = create_provider(cfg);
            Ok(Arc::from(provider))
        }
    }
}

#[async_trait]
impl SupervisorServices for WorkerSupervisorServices {
    fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn load_task(&self, task_id: String) -> Result<Task, String> {
        self.rpc.load_task(task_id).await
    }

    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError> {
        let cred = self.credentials.per_role.get(&role_kind).ok_or_else(|| {
            StageError::ModelResolution(format!(
                "no credential mounted for role {}",
                role_kind.as_str()
            ))
        })?;
        let model_id = spec.model_id_per_role.get(&role_kind).cloned().ok_or_else(|| {
            StageError::ModelResolution(format!(
                "no model assigned for role {} in TaskRunSpec",
                role_kind.as_str()
            ))
        })?;
        let context_window = self
            .get_model_context_window(model_id.clone())
            .await
            .unwrap_or(0)
            .max(0) as u32;
        let provider = build_provider_from_serializable(cred, &model_id, context_window)?;

        worker_execute_stage(
            task,
            workspace,
            role_kind,
            task_run_id,
            spec,
            self.agent_context.clone(),
            self.cancel.clone(),
            provider,
            self,
        )
        .await
    }

    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome {
        self.rpc.open_pr(spec, task).await
    }

    async fn create_task_run(
        &self,
        params: SerializableCreateTaskRunParams,
    ) -> Result<(), String> {
        self.rpc.create_task_run(params).await
    }

    async fn update_task_run_status(
        &self,
        run_id: String,
        status: TaskRunStatus,
    ) -> Result<(), String> {
        self.rpc.update_task_run_status(run_id, status).await
    }

    async fn get_model_context_window(&self, model_id: String) -> Result<i64, String> {
        self.rpc.get_model_context_window(model_id).await
    }

    async fn get_provider_base_url(
        &self,
        catalog_provider_id: String,
    ) -> Result<String, String> {
        self.rpc.get_provider_base_url(catalog_provider_id).await
    }

    async fn pick_any_default_model(&self) -> Result<Option<String>, String> {
        self.rpc.pick_any_default_model().await
    }

    async fn create_session(
        &self,
        params: SerializableCreateSessionParams,
    ) -> Result<SessionRecord, String> {
        self.rpc.create_session(params).await
    }

    async fn publish_session_message(
        &self,
        session_id: String,
        task_id: String,
        agent_type: String,
        message: serde_json::Value,
    ) -> Result<(), String> {
        self.rpc
            .publish_session_message(session_id, task_id, agent_type, message)
            .await
    }

    async fn get_environment_config(
        &self,
        project_id: String,
    ) -> Result<EnvironmentConfig, String> {
        match self.rpc.get_environment_config(project_id).await {
            Ok(cfg) => Ok(cfg),
            // Mirror the host-side degrade-to-empty semantics; the worker
            // should never hard-fail the stage on an environment_config gap.
            Err(_) => Ok(EnvironmentConfig::empty()),
        }
    }

    async fn invoke_llm(
        &self,
        model_id: String,
        conversation: Conversation,
        tools: Vec<serde_json::Value>,
        tool_choice: Option<ToolChoice>,
    ) -> Result<LlmResponse, String> {
        // The worker no longer routes its own provider calls through this RPC
        // — Phase 7b builds providers in-Pod. The method is kept callable for
        // symmetry with `DirectServices` (host-side chat-tool invocations).
        self.rpc
            .invoke_llm(model_id, conversation, tools, tool_choice)
            .await
    }

    async fn update_session_status(
        &self,
        session_id: String,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
    ) -> Result<(), String> {
        self.rpc
            .update_session_status(session_id, status, tokens_in, tokens_out)
            .await
    }
}
