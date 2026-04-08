mod fuzzy;
pub(crate) mod github_search;
pub(crate) mod handlers;
mod helpers;
pub(crate) mod shared_schemas;
pub(crate) mod tool_defs;
mod types;

use std::path::Path;

use crate::context::AgentContext;
use crate::mcp_client::McpToolRegistry;

// Re-export the public API so external callers see the same paths as before.
pub(crate) use tool_defs::{
    tool_schemas_architect, tool_schemas_lead, tool_schemas_planner, tool_schemas_reviewer,
    tool_schemas_worker,
};

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
pub(crate) async fn call_tool(
    state: &AgentContext,
    name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    session_task_id: Option<&str>,
    session_role: Option<&str>,
    mcp_registry: Option<&McpToolRegistry>,
) -> Result<serde_json::Value, String> {
    let synthetic = serde_json::json!({ "name": name, "arguments": arguments });
    handlers::dispatch_tool_call(
        state,
        &synthetic,
        worktree_path,
        None,
        session_task_id,
        session_role,
        mcp_registry,
    )
    .await
}

// Re-export sandbox at the super level for handlers.
use super::sandbox;

#[cfg(test)]
mod tests;
