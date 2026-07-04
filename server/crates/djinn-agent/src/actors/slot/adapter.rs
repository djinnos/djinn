// Shared private adapter factory used by host dispatch, reply-loop, and
// extraction adapters to build a [`djinn_slot::host::SlotContext`] from an
// [`AgentContext`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use djinn_core::clock::SystemClock;
use djinn_supervisor::SupervisorServices;

use crate::context::AgentContext;

pub(crate) fn build_slot_context(
    agent: &AgentContext,
    callbacks: Arc<dyn djinn_slot::host::SlotHostCallbacks>,
    tool_dispatcher: Option<Arc<dyn djinn_slot::host::SlotToolDispatcher>>,
) -> djinn_slot::host::SlotContext {
    djinn_slot::host::SlotContext {
        db: agent.db.clone(),
        event_bus: agent.event_bus.clone(),
        catalog: agent.catalog.clone(),
        health_tracker: agent.health_tracker.clone(),
        background_work_tasks: agent.background_work_tasks.clone(),
        active_tasks: agent.active_tasks.clone(),
        default_project_id: agent.default_project_id.clone(),
        working_root: agent.working_root.clone(),
        coordinator_trigger: None,
        runtime_ops: agent.runtime_ops.clone(),
        repo_graph_ops: agent.repo_graph_ops.clone(),
        clock: Arc::new(SystemClock::new()),
        callbacks,
        tool_dispatcher,
        compaction_cs: agent.compaction_cs.clone(),
    }
}

/// Convert `&AgentContext` into a `SlotContext` with extraction host callbacks.
pub(crate) fn agent_to_slot_context(agent: &AgentContext) -> djinn_slot::host::SlotContext {
    build_slot_context(agent, Arc::new(AgentHostCallbacks::extraction(agent)), None)
}

/// Run `f` with a temporary `SlotContext` built from `&AgentContext`. This lets
/// thin agent-side wrappers avoid repeating the `agent_to_slot_context` call.
#[macro_export]
macro_rules! with_slot_context {
    ($app_state:expr, $body:expr) => {{
        let slot_ctx = $crate::actors::slot::adapter::agent_to_slot_context($app_state);
        $body(&slot_ctx).await
    }};
}

fn agent_credential_to_slot(
    credential: super::helpers::ProviderCredential,
) -> djinn_slot::helpers::ProviderCredential {
    match credential {
        super::helpers::ProviderCredential::ApiKey(key_name, api_key) => {
            djinn_slot::helpers::ProviderCredential::ApiKey(key_name, api_key)
        }
        super::helpers::ProviderCredential::OAuthConfig(config) => {
            djinn_slot::helpers::ProviderCredential::OAuthConfig(config)
        }
    }
}

/// Shared host-callback implementation for dispatch, reply-loop, and extraction.
pub(crate) struct AgentHostCallbacks {
    agent: AgentContext,
    services: Option<&'static dyn SupervisorServices>,
    dispatch_mode: bool,
}

impl AgentHostCallbacks {
    pub(crate) fn dispatch(agent: &AgentContext) -> Self {
        Self {
            agent: agent.clone(),
            services: None,
            dispatch_mode: true,
        }
    }
    pub(crate) fn reply_loop(
        agent: &AgentContext,
        services: &'static dyn SupervisorServices,
    ) -> Self {
        Self {
            agent: agent.clone(),
            services: Some(services),
            dispatch_mode: true,
        }
    }
    pub(crate) fn extraction(agent: &AgentContext) -> Self {
        Self {
            agent: agent.clone(),
            services: None,
            dispatch_mode: false,
        }
    }
}

impl djinn_slot::host::SlotHostCallbacks for AgentHostCallbacks {
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>> + Send + 'a>>
    {
        Box::pin(async { Err("not available in host adapter".into()) })
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
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }
    fn build_mcp_state(
        &self,
        _ctx: &djinn_slot::host::SlotContext,
    ) -> djinn_control_plane::McpState {
        unreachable!("build_mcp_state not available in host adapter")
    }
    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                error: "not available in host adapter".into(),
            })
        })
    }
    fn resolve_provider_credential<'a>(
        &'a self,
        provider_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<djinn_slot::helpers::ProviderCredential, String>>
                + Send
                + 'a,
        >,
    > {
        if self.dispatch_mode {
            return Box::pin(async { Err("not available in dispatch callback".into()) });
        }
        let agent = self.agent.clone();
        Box::pin(async move {
            super::helpers::load_provider_credential(provider_id, &agent)
                .await
                .map(agent_credential_to_slot)
                .map_err(|e| {
                    format!(
                        "extraction credential resolution failed for provider {provider_id}: {e}"
                    )
                })
        })
    }
    fn run_task_dispatch<'a>(
        &'a self,
        task_id: String,
        project_path: String,
        model_id: String,
        _ctx: djinn_slot::host::SlotContext,
        kill: tokio_util::sync::CancellationToken,
        pause: tokio_util::sync::CancellationToken,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        if !self.dispatch_mode {
            return Box::pin(async { Ok(()) });
        }
        let agent = self.agent.clone();
        Box::pin(async move {
            super::supervisor_runner::dispatch_task_runtime(
                task_id,
                project_path,
                model_id,
                agent,
                kill,
                pause,
                resume_lifecycle_metadata,
            )
            .await
        })
    }
    fn touch_activity_rpc<'a>(
        &'a self,
        task_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        match self.services {
            Some(services) => Box::pin(async move { services.touch_activity(task_id).await }),
            None => Box::pin(async { Ok(()) }),
        }
    }
    fn flush_session_tokens_rpc<'a>(
        &'a self,
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        match self.services {
            Some(services) => Box::pin(async move {
                services
                    .flush_session_tokens(
                        session_id,
                        tokens_in,
                        tokens_out,
                        cache_read,
                        cache_write,
                    )
                    .await
            }),
            None => Box::pin(async { Ok(()) }),
        }
    }
}
