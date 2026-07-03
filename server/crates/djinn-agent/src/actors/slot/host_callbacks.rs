use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;

use super::adapter::build_slot_context;
use djinn_supervisor::SupervisorServices;

/// Build a dispatch-pathway [`djinn_slot::host::SlotContext`] from an [`AgentContext`].
pub(crate) fn agent_to_dispatch_slot_context(
    agent: &AgentContext,
) -> djinn_slot::host::SlotContext {
    build_slot_context(
        agent,
        Arc::new(AgentDispatchCallbacks {
            agent: agent.clone(),
            services: None,
        }),
        None,
    )
}

/// Build a reply-loop [`djinn_slot::host::SlotContext`] that routes liveness
/// and token-flush heartbeats through the live [`SupervisorServices`] handle.
pub(crate) fn agent_to_reply_loop_slot_context(
    agent: &AgentContext,
    services: &dyn SupervisorServices,
) -> djinn_slot::host::SlotContext {
    // SAFETY: the reply loop awaits every callback future before returning.
    let services_static = unsafe {
        std::mem::transmute::<&dyn SupervisorServices, &'static dyn SupervisorServices>(services)
    };
    build_slot_context(
        agent,
        Arc::new(AgentDispatchCallbacks {
            agent: agent.clone(),
            services: Some(services_static),
        }),
        None,
    )
}

struct AgentDispatchCallbacks {
    agent: AgentContext,
    services: Option<&'static dyn SupervisorServices>,
}

impl djinn_slot::host::SlotHostCallbacks for AgentDispatchCallbacks {
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
        Box::pin(async { Err("not available in dispatch callback".into()) })
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
        unreachable!("build_mcp_state not available in dispatch callback")
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
                error: "not available in dispatch callback".into(),
            })
        })
    }

    fn resolve_provider_credential<'a>(
        &'a self,
        _provider_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<djinn_slot::helpers::ProviderCredential, String>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Err("not available in dispatch callback".into()) })
    }

    fn run_task_dispatch<'a>(
        &'a self,
        task_id: String,
        project_path: String,
        model_id: String,
        _ctx: djinn_slot::host::SlotContext,
        kill: CancellationToken,
        pause: CancellationToken,
        resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        let app_state = self.agent.clone();
        Box::pin(async move {
            super::supervisor_runner::dispatch_task_runtime(
                task_id,
                project_path,
                model_id,
                app_state,
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
