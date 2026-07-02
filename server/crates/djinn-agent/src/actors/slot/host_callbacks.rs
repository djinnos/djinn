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
use djinn_supervisor::SupervisorServices;

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
        callbacks: Arc::new(AgentDispatchCallbacks {
            agent: agent.clone(),
            services: None,
        }),
        tool_dispatcher: None,
    }
}

/// Construct a reply-loop [`djinn_slot::host::SlotContext`] whose callbacks
/// carry a live [`SupervisorServices`] handle.
///
/// The canonical reply loop (djinn-slot) emits liveness (`touch_activity_rpc`)
/// and mid-flight token (`flush_session_tokens_rpc`) heartbeats through
/// `SlotHostCallbacks`. In the dispatch-only [`agent_to_dispatch_slot_context`]
/// those are inert stubs, but the reply loop MUST route them to the host:
///
/// - host (in-process): `DirectServices` writes the host's shared
///   `ActivityTracker` / session row directly.
/// - K8s worker pod: `WorkerSupervisorServices` forwards them over RPC to the
///   host, where `DirectServices` applies them.
///
/// Wiring these through `services` restores the pre-slot-extraction contract
/// (see `SupervisorServices::touch_activity` docs). Without it the worker's
/// heartbeats were dropped on the floor and the coordinator's stall poller —
/// reading its own untouched tracker — false-killed every worker session that
/// ran past the 30-minute idle threshold.
pub(crate) fn agent_to_reply_loop_slot_context(
    agent: &AgentContext,
    services: &dyn SupervisorServices,
) -> djinn_slot::host::SlotContext {
    let mut ctx = agent_to_dispatch_slot_context(agent);
    // SAFETY: mirrors `AgentToolDispatcher::new`'s lifetime discipline — the
    // reply loop awaits every callback future before returning, so this erased
    // `'static` reference never outlives the borrow it was created from.
    let services_static = unsafe {
        std::mem::transmute::<&dyn SupervisorServices, &'static dyn SupervisorServices>(services)
    };
    ctx.callbacks = Arc::new(AgentDispatchCallbacks {
        agent: agent.clone(),
        services: Some(services_static),
    });
    ctx
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
///
/// `services` is `Some` only on the reply-loop path (built via
/// [`agent_to_reply_loop_slot_context`]); there `touch_activity_rpc` and
/// `flush_session_tokens_rpc` route through it to the host. On the dispatch
/// path it is `None` and those heartbeats stay inert stubs (they are never
/// invoked there).
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
        let app_state = self.agent.clone();
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

    fn touch_activity_rpc<'a>(
        &'a self,
        task_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        // Reply-loop path: route the liveness heartbeat to the host so the
        // coordinator's stall poller sees the session is alive. Dispatch path
        // (`services == None`): inert — never invoked there.
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
        // Reply-loop path: persist mid-flight token counters to the session row
        // so long-running sessions expose real progress (also feeds the stall
        // backstop's DB-visible-progress signal). Dispatch path: inert.
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
