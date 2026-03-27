//! MCP client support for specialist agent sessions.
//!
//! Connects to resolved MCP servers at session start, discovers their tool
//! definitions via `tools/list`, and provides dispatch for tool calls routed
//! to those servers during the reply loop.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};

use crate::verification::settings::McpServerConfig;

/// Registry of MCP tool names → server connections built at session start.
///
/// Holds live `Peer<RoleClient>` handles and the tool-name→server-name mapping
/// so the reply loop can route unknown tool calls to the correct MCP server.
#[derive(Clone)]
pub(crate) struct McpToolRegistry {
    /// tool_name → server_name
    tool_to_server: HashMap<String, String>,
    /// server_name → live peer handle
    peers: HashMap<String, Arc<Peer<RoleClient>>>,
    /// All discovered tool schemas ready to append to the session tool list.
    tool_schemas: Vec<serde_json::Value>,
}

impl McpToolRegistry {
    /// Returns true if this registry has a tool with the given name.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tool_to_server.contains_key(name)
    }

    /// Returns the discovered tool schemas (provider-compatible JSON).
    pub fn tool_schemas(&self) -> &[serde_json::Value] {
        &self.tool_schemas
    }

    /// Dispatch a tool call to the MCP server that owns the given tool name.
    ///
    /// Returns `Ok(json)` on success or `Err(message)` on failure.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value, String> {
        let server_name = self
            .tool_to_server
            .get(tool_name)
            .ok_or_else(|| format!("MCP tool `{tool_name}` not found in registry"))?;

        let peer = self
            .peers
            .get(server_name.as_str())
            .ok_or_else(|| format!("MCP server `{server_name}` peer not found"))?;

        let params = CallToolRequestParams {
            name: tool_name.to_string().into(),
            arguments: arguments.map(|m| m.into_iter().collect()),
            meta: None,
            task: None,
        };

        let result = peer.call_tool(params).await.map_err(|e| {
            format!("MCP tool call `{tool_name}` on server `{server_name}` failed: {e}")
        })?;

        call_tool_result_to_json(result)
    }
}

/// Convert a `CallToolResult` into a JSON value suitable for the reply loop.
fn call_tool_result_to_json(result: CallToolResult) -> Result<serde_json::Value, String> {
    // CallToolResult has `content` (Vec<Content>) and `is_error` (Option<bool>).
    let is_error = result.is_error.unwrap_or(false);

    // Collect text content from the result.
    let mut text_parts: Vec<String> = Vec::new();
    for content in &result.content {
        // rmcp Content can be Text, Image, Resource, etc.
        // We extract text content and serialize others as JSON.
        if let Ok(val) = serde_json::to_value(content) {
            if let Some(text) = val.get("text").and_then(|t| t.as_str()) {
                text_parts.push(text.to_string());
            } else {
                // Non-text content: serialize the whole thing
                text_parts.push(val.to_string());
            }
        }
    }

    let combined = text_parts.join("\n");

    if is_error {
        Err(combined)
    } else {
        // Try to parse as JSON first; fall back to string.
        match serde_json::from_str::<serde_json::Value>(&combined) {
            Ok(val) => Ok(val),
            Err(_) => Ok(serde_json::json!({ "result": combined })),
        }
    }
}

/// Connect to resolved MCP servers and discover their tools.
///
/// For each `(name, config)` pair:
/// 1. If the config has a `url`, connect via Streamable HTTP transport.
/// 2. Call `tools/list` on the connected peer.
/// 3. Convert each MCP tool definition into a provider-compatible JSON schema.
///
/// Servers that fail to connect or list tools are logged and skipped (non-fatal).
/// Returns `None` when no tools were discovered.
pub(crate) async fn connect_and_discover(
    task_short_id: &str,
    role_name: &str,
    servers: &[(String, McpServerConfig)],
) -> Option<McpToolRegistry> {
    if servers.is_empty() {
        return None;
    }

    let mut tool_to_server: HashMap<String, String> = HashMap::new();
    let mut peers: HashMap<String, Arc<Peer<RoleClient>>> = HashMap::new();
    let mut tool_schemas: Vec<serde_json::Value> = Vec::new();

    for (name, config) in servers {
        let url = match &config.url {
            Some(url) => url.clone(),
            None => {
                // stdio transport not yet supported for agent sessions; skip.
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    "MCP server has no URL (stdio not yet supported for agent sessions); skipping"
                );
                continue;
            }
        };

        // Connect to the MCP server.
        let peer = match connect_to_server(&url).await {
            Ok(peer) => {
                tracing::info!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    url = %url,
                    "Connected to MCP server"
                );
                Arc::new(peer)
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    url = %url,
                    error = %e,
                    "Failed to connect to MCP server; skipping"
                );
                continue;
            }
        };

        // Discover tools from this server.
        match peer.list_tools(None).await {
            Ok(result) => {
                let tool_count = result.tools.len();
                for tool in result.tools {
                    let tool_name = tool.name.clone();

                    // Convert rmcp Tool to provider-compatible JSON schema.
                    let schema = match serde_json::to_value(&tool) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                task_id = %task_short_id,
                                server = %name,
                                tool = %tool_name,
                                error = %e,
                                "Failed to serialize MCP tool schema; skipping tool"
                            );
                            continue;
                        }
                    };

                    if tool_to_server.contains_key(&*tool_name) {
                        tracing::warn!(
                            task_id = %task_short_id,
                            server = %name,
                            tool = %tool_name,
                            "Duplicate MCP tool name; later server wins"
                        );
                    }

                    tool_to_server.insert(tool_name.to_string(), name.clone());
                    tool_schemas.push(schema);
                }
                peers.insert(name.clone(), peer);
                tracing::info!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    tool_count,
                    "Discovered MCP tools"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    error = %e,
                    "Failed to list tools from MCP server; skipping"
                );
            }
        }
    }

    if tool_schemas.is_empty() {
        return None;
    }

    Some(McpToolRegistry {
        tool_to_server,
        peers,
        tool_schemas,
    })
}

/// Establish a connection to an MCP server via Streamable HTTP transport.
async fn connect_to_server(url: &str) -> Result<Peer<RoleClient>, String> {
    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = ()
        .serve(transport)
        .await
        .map_err(|e| format!("MCP transport handshake failed: {e}"))?;
    let peer = service.peer().clone();
    // Keep the service alive in the background.
    tokio::spawn(async move {
        let _ = service.waiting().await;
    });
    Ok(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_tool_result_text_content() {
        use rmcp::model::Content;

        let result = CallToolResult {
            content: vec![Content::text("hello world")],
            is_error: None,
            meta: None,
            structured_content: None,
        };
        let json = call_tool_result_to_json(result).unwrap();
        assert_eq!(json, serde_json::json!({ "result": "hello world" }));
    }

    #[test]
    fn call_tool_result_json_content() {
        use rmcp::model::Content;

        let result = CallToolResult {
            content: vec![Content::text(r#"{"key": "value"}"#)],
            is_error: None,
            meta: None,
            structured_content: None,
        };
        let json = call_tool_result_to_json(result).unwrap();
        assert_eq!(json, serde_json::json!({ "key": "value" }));
    }

    #[test]
    fn call_tool_result_error() {
        use rmcp::model::Content;

        let result = CallToolResult {
            content: vec![Content::text("something went wrong")],
            is_error: Some(true),
            meta: None,
            structured_content: None,
        };
        let err = call_tool_result_to_json(result).unwrap_err();
        assert_eq!(err, "something went wrong");
    }

    #[test]
    fn empty_registry_has_no_tools() {
        let registry = McpToolRegistry {
            tool_to_server: HashMap::new(),
            peers: HashMap::new(),
            tool_schemas: Vec::new(),
        };
        assert!(!registry.has_tool("anything"));
        assert!(registry.tool_schemas().is_empty());
    }

    #[test]
    fn registry_lookup() {
        let mut tool_to_server = HashMap::new();
        tool_to_server.insert("web_search".to_string(), "search-server".to_string());

        let registry = McpToolRegistry {
            tool_to_server,
            peers: HashMap::new(),
            tool_schemas: vec![serde_json::json!({"name": "web_search"})],
        };
        assert!(registry.has_tool("web_search"));
        assert!(!registry.has_tool("unknown_tool"));
        assert_eq!(registry.tool_schemas().len(), 1);
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let registry = McpToolRegistry {
            tool_to_server: HashMap::new(),
            peers: HashMap::new(),
            tool_schemas: Vec::new(),
        };
        let result = registry.call_tool("nonexistent", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found in registry"));
    }

    #[tokio::test]
    async fn connect_and_discover_empty_servers() {
        let result = connect_and_discover("test", "worker", &[]).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn connect_and_discover_skips_stdio_only() {
        let servers = vec![(
            "stdio-server".to_string(),
            McpServerConfig {
                url: None,
                command: Some("my-server".to_string()),
            },
        )];
        let result = connect_and_discover("test", "worker", &servers).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn connect_and_discover_skips_unreachable() {
        let servers = vec![(
            "bad-server".to_string(),
            McpServerConfig {
                url: Some("http://127.0.0.1:1/mcp".to_string()),
                command: None,
            },
        )];
        let result = connect_and_discover("test", "worker", &servers).await;
        assert!(result.is_none());
    }
}
