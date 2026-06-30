// ─── hfhw cutover: host callbacks for the djinn-slot dispatch pathway ─────
//
// After the htfw cutover, the slot actor dispatches through
// `djinn_slot::run_supervisor_dispatch` → `SlotHostCallbacks::run_task_dispatch`.
// This module provides the concrete `AgentDispatchCallbacks` implementation
// that wraps `AgentContext` and contains the host-side dispatch entry point.
//
// The actual dispatch logic lives in `super::supervisor_runner::dispatch_task_runtime`
// (called from the `run_task_dispatch` callback).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;

/// Construct a [`djinn_slot::host::SlotContext`] from an [`AgentContext`] for
/// the dispatch pathway.
///
/// Provides a `SlotContext` whose callbacks delegate to the host-side dispatch
/// logic (`dispatch_task_runtime`). Other `SlotHostCallbacks` methods are stubs
/// because the host-side dispatch path uses `AgentContext` directly for MCP
/// resolution, prompt rendering, etc.
pub(crate) fn agent_to_dispatch_slot_context(
    agent: &AgentContext,
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
        clock: Arc::new(djinn_core::clock::SystemClock::new()),
        callbacks: Arc::new(AgentDispatchCallbacks(agent.clone())),
    }
}

/// Host callback implementation for the dispatch pathway.
///
/// Wraps [`AgentContext`] so `run_task_dispatch` can invoke the host-side
/// dispatch logic (`super::supervisor_runner::dispatch_task_runtime`) which
/// depends on many `djinn-agent` modules (runtime_bridge, supervisor,
/// lifecycle stages, reply_loop, etc.).
///
/// Other `SlotHostCallbacks` methods are stubs: the host-side dispatch path
/// uses `AgentContext` directly (not through callbacks) for MCP resolution,
/// prompt rendering, provider credentials, etc.
struct AgentDispatchCallbacks(AgentContext);

impl djinn_slot::host::SlotHostCallbacks for AgentDispatchCallbacks {
    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        // Not invoked in the dispatch pathway — the host handles interrupts
        // through the slot actor's kill token.
        Box::pin(async {})
    }

    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<djinn_slot::host::ResolvedMcpTools, String>> + Send + 'a>>
    {
        // Not invoked in the dispatch pathway — MCP resolution happens inside
        // the supervisor/stage execution which uses AgentContext directly.
        Box::pin(async { Err("not available in dispatch callback".into()) })
    }

    fn render_prompt(
        &self,
        _role_name: &str,
        _task: &djinn_core::models::Task,
        _context_json: &serde_json::Value,
    ) -> String {
        // Not invoked in the dispatch pathway — prompt rendering happens inside
        // stage.rs via `assemble_prompt_context` which uses AgentContext directly.
        String::new()
    }

    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a djinn_slot::host::SlotContext,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        // Not invoked in the dispatch pathway.
        Box::pin(async { String::new() })
    }

    fn build_mcp_state(
        &self,
        _ctx: &djinn_slot::host::SlotContext,
    ) -> djinn_control_plane::McpState {
        // Not invoked in the dispatch pathway.
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
        // Not invoked in the dispatch pathway — credential resolution happens
        // inside dispatch_task_runtime which uses AgentContext directly.
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
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        let app_state = self.0.clone();
        Box::pin(async move {
            super::supervisor_runner::dispatch_task_runtime(
                task_id,
                project_path,
                model_id,
                app_state,
                kill,
                pause,
            )
            .await
        })
    }
}
