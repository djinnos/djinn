//! Chat-specific tool surface.
//!
//! Exposes a subset of the agent extension tools (shell, read, lsp,
//! code_graph, github_search) for use in the Djinn chat interface. The chat
//! runs in the project root directory (not a task worktree), so
//! write/edit/apply_patch are excluded.
//!
//! Per ADR-050, the chat is the interactive form of the Architect: any
//! code-reading or analysis capability granted to the Architect must also be
//! present here, and vice versa. The `code_graph` tool is the most recent
//! addition under that contract.

use std::path::Path;

use crate::context::AgentContext;

/// Tool schemas for the chat interface: read-only codebase tools.
pub fn chat_extension_tool_schemas() -> Vec<serde_json::Value> {
    use crate::extension::tool_defs::{
        tool_code_graph, tool_github_search, tool_lsp, tool_read, tool_shell,
    };
    vec![
        serde_json::to_value(tool_shell()).expect("serialize tool_shell"),
        serde_json::to_value(tool_read()).expect("serialize tool_read"),
        serde_json::to_value(tool_lsp()).expect("serialize tool_lsp"),
        serde_json::to_value(tool_code_graph()).expect("serialize tool_code_graph"),
        serde_json::to_value(tool_github_search()).expect("serialize tool_github_search"),
    ]
}

/// Names of tools that [`dispatch_chat_tool`] can handle.
const CHAT_EXTENSION_TOOLS: &[&str] = &["shell", "read", "lsp", "code_graph", "github_search"];

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
    // ADR-050 Chunk C: respect `working_root` if the caller installed one on
    // the AgentContext (e.g. the chat first-use hook resolves the canonical
    // index-tree path).  Falls back to the supplied `project_root`.
    let effective_root = state.working_root_for(project_root);
    let effective_root_str = effective_root.to_string_lossy().into_owned();
    match name {
        "shell" => handlers::call_shell(&arguments, &effective_root).await,
        "read" => handlers::call_read(state, &arguments, &effective_root).await,
        "lsp" => handlers::call_lsp(state, &arguments, &effective_root).await,
        "code_graph" => handlers::call_code_graph(state, &arguments, &effective_root_str).await,
        "github_search" => handlers::call_github_search(state, &arguments).await,
        _ => Err(format!("unknown chat extension tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::tool_defs::tool_schemas_architect;
    use std::collections::BTreeSet;

    fn schema_names(schemas: &[serde_json::Value]) -> BTreeSet<String> {
        schemas
            .iter()
            .filter_map(|v| {
                v.get("name")
                    .and_then(|n| n.as_str())
                    .map(ToString::to_string)
            })
            .collect()
    }

    /// ADR-050 §2 parity contract: any code-reading or analysis capability granted
    /// to the Architect must also be present in Chat, and vice versa. This test
    /// codifies the symmetric subset and fails loudly if either surface ever loses
    /// one of these tools.
    #[test]
    fn architect_and_chat_share_read_and_analysis_tools() {
        let symmetric: &[&str] = &["shell", "read", "lsp", "code_graph", "github_search"];

        let architect = schema_names(&tool_schemas_architect());
        let chat = schema_names(&chat_extension_tool_schemas());

        for tool in symmetric {
            assert!(
                architect.contains(*tool),
                "ADR-050 parity violation: Architect tool surface is missing `{tool}`. \
                 The Architect and Chat must share the read/analysis tool subset \
                 ({symmetric:?}). Update tool_schemas_architect() in extension/tool_defs.rs."
            );
            assert!(
                chat.contains(*tool),
                "ADR-050 parity violation: Chat tool surface is missing `{tool}`. \
                 The Architect and Chat must share the read/analysis tool subset \
                 ({symmetric:?}). Update chat_extension_tool_schemas() in chat_tools.rs."
            );
        }
    }
}
