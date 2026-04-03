//! Chat-specific tool surface.
//!
//! Exposes a subset of the agent extension tools (shell, read, lsp,
//! github_search) for use in the Djinn chat interface. The chat runs in the
//! project root directory (not a task worktree), so write/edit/apply_patch are
//! excluded.

use std::path::Path;

use crate::context::AgentContext;

/// Tool schemas for the chat interface: read-only codebase tools.
pub fn chat_extension_tool_schemas() -> Vec<serde_json::Value> {
    use crate::extension::tool_defs::{tool_github_search, tool_lsp, tool_read, tool_shell};
    vec![
        serde_json::to_value(tool_shell()).expect("serialize tool_shell"),
        serde_json::to_value(tool_read()).expect("serialize tool_read"),
        serde_json::to_value(tool_lsp()).expect("serialize tool_lsp"),
        serde_json::to_value(tool_github_search()).expect("serialize tool_github_search"),
    ]
}

/// Names of tools that [`dispatch_chat_tool`] can handle.
const CHAT_EXTENSION_TOOLS: &[&str] = &["shell", "read", "lsp", "github_search"];

/// Returns `true` if this tool name is handled by the chat extension dispatch.
pub fn is_chat_extension_tool(name: &str) -> bool {
    CHAT_EXTENSION_TOOLS.contains(&name)
}

/// Dispatch a chat extension tool call.
///
/// `project_root` is the project's root directory — the equivalent of a
/// worktree path for agents.
pub async fn dispatch_chat_tool(
    state: &AgentContext,
    name: &str,
    args: serde_json::Value,
    project_root: &Path,
) -> Result<serde_json::Value, String> {
    use crate::extension::handlers;

    let arguments = args.as_object().cloned();
    match name {
        "shell" => handlers::call_shell(&arguments, project_root).await,
        "read" => handlers::call_read(state, &arguments, project_root).await,
        "lsp" => handlers::call_lsp(state, &arguments, project_root).await,
        "github_search" => handlers::call_github_search(state, &arguments).await,
        _ => Err(format!("unknown chat extension tool: {name}")),
    }
}
