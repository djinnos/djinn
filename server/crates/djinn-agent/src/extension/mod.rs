mod fuzzy;
pub(crate) mod github_search;
pub(crate) mod handlers;
// Retained for test coverage of handlers that were migrated to djinn-mcp-extension.
// The dead_code lint is suppressed because these helpers are only called from
// handler functions that are themselves test-only (dispatched through the
// djinn-mcp-extension facade in production).
#[allow(dead_code)]
mod helpers;
#[allow(dead_code)]
mod types;

// Façade: re-export schema surfaces from `djinn-mcp-extension` so that
// existing callers see the same paths as before the extraction.
// The schema-only modules now live in `djinn-mcp-extension`; handler
// dispatch and parameter types remain in this crate.
pub(crate) use djinn_mcp_extension::shared_schemas;
pub(crate) use djinn_mcp_extension::tool_defs;
// `tool_defs_code_graph` items are re-exported through `tool_defs`.

// Re-export the public API so external callers see the same paths as before.
#[allow(unused_imports)] // evidence_spike_tool_names is for downstream callers
pub(crate) use djinn_mcp_extension::tool_defs::{
    evidence_spike_tool_names, tool_schemas_adversary, tool_schemas_advocate,
    tool_schemas_architect, tool_schemas_evidence_spike, tool_schemas_judge, tool_schemas_lead,
    tool_schemas_planner, tool_schemas_reviewer, tool_schemas_worker,
};

// Façade: re-export the public surface of `djinn-mcp-extension` so that
// existing callers see the same paths once code is migrated there.
#[allow(unused_imports)]
pub use djinn_mcp_extension as mcp_ext;

use std::path::Path;

use crate::context::AgentContext;
use crate::mcp_client::McpToolRegistry;
use tokio_util::sync::CancellationToken;

/// Agent-private cancellation pair for tool dispatch.
///
/// Retains cloned session and global cancellation tokens on the concrete
/// `AgentToolDispatcher` so cancellation can flow to the shell runner without
/// modifying the shared `SlotToolDispatcher` trait or storing tokens broadly
/// on `AgentContext`. Only the local shell handler acts on cancellation; other
/// extension handlers preserve behavior and ignore it.
#[derive(Clone)]
pub(crate) struct ToolCancellation {
    pub(crate) session: CancellationToken,
    pub(crate) global: CancellationToken,
}

impl ToolCancellation {
    pub(crate) fn new(session: CancellationToken, global: CancellationToken) -> Self {
        Self { session, global }
    }

    /// A pair of never-cancelled tokens, for tests and non-cancelling callers.
    #[cfg(test)]
    pub(crate) fn never() -> Self {
        Self::new(CancellationToken::new(), CancellationToken::new())
    }

    /// Synchronous check: cancelled if either the session or global token fired.
    #[allow(dead_code)] // consumed by T3/T4 telemetry/producer wiring
    pub(crate) fn is_cancelled(&self) -> bool {
        self.session.is_cancelled() || self.global.is_cancelled()
    }
}

/// Public entry point for the Djinn-native reply loop to call a tool by name.
///
/// `arguments` should be the `input` field from a `ContentBlock::ToolUse`
/// converted to an `Option<Map>`:
///
/// ```rust,ignore
/// let args = match input {
///     Value::Object(map) => Some(map),
///     _ => None,
/// };
/// ```
///
/// `allowed_schemas`, when provided, restricts the dispatch to only those
/// tools whose names appear in the schema list.  This is the defense-in-depth
/// enforcement for the evidence-spike read-only profile: the primary
/// restriction is at stage time (the LLM only sees read-only tools), but
/// this gate rejects any mutation tool that reaches dispatch.
// Public tool-call entrypoint mirroring `dispatch_tool_call`; each arg is a
// distinct collaborator/context, so a bag struct adds no clarity.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn call_tool(
    state: &AgentContext,
    services: &dyn djinn_supervisor::SupervisorServices,
    name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    session_task_id: Option<&str>,
    session_role: Option<&str>,
    mcp_registry: Option<&McpToolRegistry>,
    allowed_schemas: Option<&[serde_json::Value]>,
    cancel: &ToolCancellation,
) -> djinn_core::tool_call::ToolCallOutcome {
    let synthetic = serde_json::json!({ "name": name, "arguments": arguments });

    // Try the extension dispatch first. It handles most tools through
    // ExtensionContext and returns Unhandled for tools that need djinn-agent
    // internals (workspace, task_merge, coordinator, code_graph, skill_read).
    let ext_result = djinn_mcp_extension::dispatch::dispatch_tool_call(
        state as &dyn djinn_mcp_extension::ExtensionContext,
        services,
        &synthetic,
        worktree_path,
        allowed_schemas,
        session_task_id,
        session_role,
    )
    .await;

    match ext_result {
        djinn_mcp_extension::DispatchResult::Handled(outcome) => outcome,
        djinn_mcp_extension::DispatchResult::Unhandled(prepared) => {
            // Fall back to the local handler for tools that need djinn-agent
            // internals (workspace ops, task_merge, coordinator, code_graph,
            // skill_read).
            handlers::dispatch_tool_call(
                state,
                services,
                prepared,
                worktree_path,
                allowed_schemas,
                session_task_id,
                session_role,
                mcp_registry,
                cancel,
            )
            .await
        }
    }
}

// Re-export sandbox at the super level for handlers.
use super::sandbox;

#[cfg(test)]
mod tests;
