//! Reply-loop facade: adapts agent host services into djinn-slot SlotContext.

use std::sync::{Arc, Mutex};

use djinn_provider::message::Conversation;

use crate::context::AgentContext;
use crate::output_parser::ParsedAgentOutput;
use crate::output_stash::{OutputStash, handle_stash_tool, is_stash_tool, render_tool_result};

pub(crate) mod error_handling {
    pub(crate) use djinn_slot::reply_loop::error_handling::*;
}

pub(crate) mod loop_guard {
    pub(crate) use djinn_slot::reply_loop::loop_guard::*;
}

pub(crate) struct ReplyLoopContext<'a> {
    pub provider: &'a dyn djinn_provider::provider::LlmProvider,
    pub tools: &'a [serde_json::Value],
    pub task_id: &'a str,
    pub task_short_id: &'a str,
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub role_name: &'a str,
    pub finalize_tool_names: &'a [&'a str],
    pub context_window: i64,
    pub model_id: &'a str,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub global_cancel: &'a tokio_util::sync::CancellationToken,
    pub app_state: &'a AgentContext,
    pub services: &'a dyn djinn_supervisor::SupervisorServices,
    pub mcp_registry: Option<&'a crate::mcp_client::McpToolRegistry>,
    pub active_skill_names: &'a [String],
    pub active_mcp_server_names: &'a [String],
    pub max_turns_override: Option<u32>,
    /// When `true`, the session runs under the evidence-spike read-only
    /// profile.  The dispatcher enforces allowed_schemas at dispatch time
    /// as defense-in-depth beyond the stage-time schema restriction.
    pub is_evidence_spike: bool,
}

struct AgentToolDispatcher {
    app_state: AgentContext,
    services: &'static dyn djinn_supervisor::SupervisorServices,
    mcp_registry: Option<&'static crate::mcp_client::McpToolRegistry>,
    output_stash: Mutex<OutputStash>,
    /// When set, only tools whose names appear in these schemas may be
    /// dispatched.  Used by the evidence-spike runtime profile to enforce
    /// read-only/fail-closed access at dispatch time (defense-in-depth
    /// beyond the stage-time schema restriction).
    allowed_schemas: Option<Vec<serde_json::Value>>,
}

impl AgentToolDispatcher {
    fn new(
        app_state: &AgentContext,
        services: &dyn djinn_supervisor::SupervisorServices,
        mcp_registry: Option<&crate::mcp_client::McpToolRegistry>,
        allowed_schemas: Option<Vec<serde_json::Value>>,
    ) -> Self {
        // SAFETY: the dispatcher is created immediately before calling the
        // canonical reply loop and dropped when that call returns. The canonical
        // loop awaits every tool-dispatch future before returning, so these
        // erased references cannot outlive their source borrows.
        let services_static = unsafe {
            std::mem::transmute::<
                &dyn djinn_supervisor::SupervisorServices,
                &'static dyn djinn_supervisor::SupervisorServices,
            >(services)
        };
        let registry_static = mcp_registry.map(|registry| unsafe {
            std::mem::transmute::<
                &crate::mcp_client::McpToolRegistry,
                &'static crate::mcp_client::McpToolRegistry,
            >(registry)
        });
        Self {
            app_state: app_state.clone(),
            services: services_static,
            mcp_registry: registry_static,
            output_stash: Mutex::new(OutputStash::new()),
            allowed_schemas,
        }
    }
}

impl djinn_slot::host::SlotToolDispatcher for AgentToolDispatcher {
    fn is_stash_tool(&self, tool_name: &str) -> bool {
        is_stash_tool(tool_name)
    }
    fn handle_stash_call(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, String> {
        handle_stash_tool(&self.output_stash, tool_name, arguments)
    }
    fn render_result(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        value: &serde_json::Value,
    ) -> String {
        render_tool_result(&self.output_stash, tool_use_id, tool_name, value)
    }
    fn dispatch_extension_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        worktree_path: &'a std::path::Path,
        task_id: &'a str,
        role_name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(crate::extension::call_tool(
            &self.app_state,
            self.services,
            tool_name,
            arguments,
            worktree_path,
            Some(task_id),
            Some(role_name),
            self.mcp_registry,
            self.allowed_schemas.as_deref(),
        ))
    }
    fn is_mcp_tool(&self, tool_name: &str) -> bool {
        self.mcp_registry
            .map(|registry| registry.has_tool(tool_name))
            .unwrap_or(false)
    }
    fn dispatch_mcp_tool<'a>(
        &'a self,
        tool_name: &'a str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        match self.mcp_registry {
            Some(registry) => Box::pin(registry.call_tool(tool_name, arguments)),
            None => Box::pin(async move { Err(format!("MCP tool `{tool_name}` not found")) }),
        }
    }
    fn mcp_server_for_tool(&self, tool_name: &str) -> Option<String> {
        self.mcp_registry
            .and_then(|registry| registry.server_for_tool(tool_name))
    }
    fn clear_stash(&self) {
        self.output_stash
            .lock()
            .expect("output stash mutex")
            .clear();
    }
}

pub(crate) async fn run_reply_loop(
    ctx: ReplyLoopContext<'_>,
    conversation: &mut Conversation,
    is_resumed_session: bool,
) -> (anyhow::Result<()>, ParsedAgentOutput, i64, i64, i64, i64) {
    let ReplyLoopContext {
        provider,
        tools,
        task_id,
        task_short_id,
        session_id,
        project_path,
        worktree_path,
        role_name,
        finalize_tool_names,
        context_window,
        model_id,
        cancel,
        global_cancel,
        app_state,
        services,
        mcp_registry,
        active_skill_names,
        active_mcp_server_names,
        max_turns_override,
        is_evidence_spike,
    } = ctx;
    let mut slot_ctx = super::host_callbacks::agent_to_reply_loop_slot_context(app_state, services);
    // Evidence-spike sessions get the restricted tool set as allowed_schemas
    // for defense-in-depth dispatch-time enforcement.  For normal sessions
    // this is None (no dispatch-time gate — the full role schema applies).
    let allowed_for_dispatch = if is_evidence_spike {
        Some(tools.to_vec())
    } else {
        None
    };
    slot_ctx.tool_dispatcher = Some(Arc::new(AgentToolDispatcher::new(
        app_state,
        services,
        mcp_registry,
        allowed_for_dispatch,
    )));
    // Shared compaction critical section for this reply-loop session; the slot
    // reply loop enters it around every context rotation and releases it on
    // every exit path.
    let compaction_cs = djinn_slot::reply_loop::CompactionCriticalSection::new();
    let (result, output, tokens_in, tokens_out, cache_read, cache_write) =
        djinn_slot::reply_loop::run_reply_loop(
            djinn_slot::reply_loop::ReplyLoopContext {
                provider,
                tools,
                task_id,
                task_short_id,
                session_id,
                project_path,
                worktree_path,
                role_name,
                finalize_tool_names,
                context_window,
                model_id,
                cancel,
                global_cancel,
                ctx: &slot_ctx,
                active_skill_names,
                active_mcp_server_names,
                max_turns_override,
                compaction_cs: &compaction_cs,
            },
            conversation,
            is_resumed_session,
        )
        .await;
    let mut agent_output = ParsedAgentOutput::new(false);
    agent_output.runtime_error = output.runtime_error;
    agent_output.reviewer_feedback = output.reviewer_feedback;
    agent_output.finalize_payload = output.finalize_payload;
    agent_output.finalize_tool_name = output.finalize_tool_name;
    agent_output.budget_wind_down_summary = output.budget_wind_down_summary;
    agent_output.budget_wind_down_details = output.budget_wind_down_details;
    (
        result,
        agent_output,
        tokens_in,
        tokens_out,
        cache_read,
        cache_write,
    )
}

// Agent-side reply-loop tests duplicated canonical behavior and are intentionally
// retired with this facade cut-over. Canonical coverage lives in
// `server/crates/djinn-slot/src/reply_loop/tests.rs` and
// `server/crates/djinn-slot/src/reply_loop_tests.rs`; compatibility is compile-
// checked through this adapter and `supervisor_impl::stage`.
