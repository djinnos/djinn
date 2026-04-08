//! MCP client support for specialist agent sessions.
//!
//! Connects to resolved MCP servers at session start, discovers their tool
//! definitions via `tools/list`, and provides dispatch for tool calls routed
//! to those servers during the reply loop.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use std::pin::Pin;

use regex::Regex;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};

use crate::context::AgentContext;
use crate::extension::shared_schemas;
use crate::verification::settings::McpServerConfig;
use djinn_provider::repos::CredentialRepository;

static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z0-9_]+)\}").expect("valid MCP placeholder regex"));

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
    #[cfg(test)]
    test_dispatch: Option<Arc<TestDispatchFn>>,
}

#[cfg(test)]
type TestDispatchFuture = Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>;

#[cfg(test)]
type TestDispatchFn = dyn Fn(&str, Option<serde_json::Map<String, serde_json::Value>>) -> TestDispatchFuture
    + Send
    + Sync;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedMcpServerConfig {
    url: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    env: HashMap<String, String>,
    headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpTransportKind {
    Http,
    Stdio,
    Unsupported,
}

impl ResolvedMcpServerConfig {
    fn transport_kind(&self) -> McpTransportKind {
        if self.url.is_some() {
            McpTransportKind::Http
        } else if self.command.is_some() {
            McpTransportKind::Stdio
        } else {
            McpTransportKind::Unsupported
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MissingPlaceholder {
    field: String,
    variable: String,
}

enum PlaceholderLookup {
    Found(String),
    Missing,
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
        #[cfg(test)]
        if let Some(dispatch) = &self.test_dispatch {
            return dispatch(tool_name, arguments).await;
        }

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
/// 1. Resolve `${VAR_NAME}` placeholders against environment/credentials.
/// 2. If the config resolves to HTTP, connect via Streamable HTTP transport.
/// 3. Call `tools/list` on the connected peer.
/// 4. Convert each MCP tool definition into a provider-compatible JSON schema.
///
/// Servers that fail to resolve, connect, or list tools are logged and skipped (non-fatal).
/// Returns `None` when no tools were discovered.
pub(crate) async fn connect_and_discover(
    task_short_id: &str,
    role_name: &str,
    servers: &[(String, McpServerConfig)],
    app_state: &AgentContext,
) -> Option<McpToolRegistry> {
    if servers.is_empty() {
        return None;
    }

    let mut tool_to_server: HashMap<String, String> = HashMap::new();
    let mut peers: HashMap<String, Arc<Peer<RoleClient>>> = HashMap::new();
    let mut tool_schemas: Vec<serde_json::Value> = Vec::new();

    for (name, config) in servers {
        let resolved = match resolve_server_config(name, config, app_state).await {
            Ok(resolved) => resolved,
            Err(missing) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    field = %missing.field,
                    variable = %missing.variable,
                    "MCP server config references missing placeholder value; skipping"
                );
                continue;
            }
        };

        let url = match resolved.transport_kind() {
            McpTransportKind::Http => resolved.url.clone().expect("HTTP transport requires URL"),
            McpTransportKind::Stdio => {
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    has_command = true,
                    arg_count = resolved.args.len(),
                    env_count = resolved.env.len(),
                    "MCP server uses stdio transport (not yet supported for agent sessions); skipping"
                );
                continue;
            }
            McpTransportKind::Unsupported => {
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    "MCP server config has neither URL nor command; skipping unsupported transport"
                );
                continue;
            }
        };

        // Connect to the MCP server.
        let peer = match connect_to_server(&url, &resolved.headers).await {
            Ok(peer) => {
                tracing::info!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    url = %url,
                    header_count = resolved.headers.len(),
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
                        Ok(mut v) => {
                            shared_schemas::annotate_concurrent_safe(&mut v, false);
                            v
                        }
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
        #[cfg(test)]
        test_dispatch: None,
    })
}

async fn resolve_server_config(
    server_name: &str,
    config: &McpServerConfig,
    app_state: &AgentContext,
) -> Result<ResolvedMcpServerConfig, MissingPlaceholder> {
    Ok(ResolvedMcpServerConfig {
        url: match &config.url {
            Some(url) => Some(
                resolve_placeholder_value(app_state, url, &format!("server `{server_name}` url"))
                    .await?,
            ),
            None => None,
        },
        command: config.command.clone(),
        args: config.args.clone(),
        env: resolve_placeholder_map(
            app_state,
            &config.env,
            &format!("server `{server_name}` env"),
        )
        .await?,
        headers: resolve_placeholder_map(
            app_state,
            &config.headers,
            &format!("server `{server_name}` header"),
        )
        .await?,
    })
}

async fn resolve_placeholder_map(
    app_state: &AgentContext,
    values: &HashMap<String, String>,
    field_prefix: &str,
) -> Result<HashMap<String, String>, MissingPlaceholder> {
    let mut resolved = HashMap::with_capacity(values.len());
    for (key, value) in values {
        resolved.insert(
            key.clone(),
            resolve_placeholder_value(app_state, value, &format!("{field_prefix} `{key}`")).await?,
        );
    }
    Ok(resolved)
}

async fn resolve_placeholder_value(
    app_state: &AgentContext,
    value: &str,
    field: &str,
) -> Result<String, MissingPlaceholder> {
    let mut resolved = String::with_capacity(value.len());
    let mut last_end = 0;

    for captures in PLACEHOLDER_RE.captures_iter(value) {
        let full = captures.get(0).expect("full placeholder match");
        let variable = captures
            .get(1)
            .expect("placeholder variable capture")
            .as_str();

        resolved.push_str(&value[last_end..full.start()]);
        match lookup_placeholder_value(app_state, variable).await {
            PlaceholderLookup::Found(replacement) => resolved.push_str(&replacement),
            PlaceholderLookup::Missing => {
                return Err(MissingPlaceholder {
                    field: field.to_string(),
                    variable: variable.to_string(),
                });
            }
        }
        last_end = full.end();
    }

    if last_end == 0 {
        return Ok(value.to_string());
    }

    resolved.push_str(&value[last_end..]);
    Ok(resolved)
}

async fn lookup_placeholder_value(app_state: &AgentContext, variable: &str) -> PlaceholderLookup {
    if let Ok(value) = std::env::var(variable) {
        return PlaceholderLookup::Found(value);
    }

    let credential_repo =
        CredentialRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match credential_repo.get_decrypted(variable).await {
        Ok(Some(value)) => PlaceholderLookup::Found(value),
        Ok(None) => PlaceholderLookup::Missing,
        Err(error) => {
            tracing::warn!(
                variable = variable,
                error = %error,
                "Failed to resolve MCP placeholder from credential store"
            );
            PlaceholderLookup::Missing
        }
    }
}

/// Establish a connection to an MCP server via Streamable HTTP transport.
async fn connect_to_server(
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<Peer<RoleClient>, String> {
    let mut custom_headers = HashMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::try_from(name.as_str())
            .map_err(|e| format!("invalid header name `{name}` for `{url}`: {e}"))?;
        let header_value = HeaderValue::try_from(value.as_str())
            .map_err(|e| format!("invalid header value for `{name}` on `{url}`: {e}"))?;
        custom_headers.insert(header_name, header_value);
    }

    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
        .custom_headers(custom_headers);
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
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use djinn_core::events::EventBus;
    use djinn_provider::repos::CredentialRepository;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    fn test_context() -> AgentContext {
        agent_context_from_db(create_test_db(), CancellationToken::new())
    }

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
            test_dispatch: None,
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
            test_dispatch: None,
        };
        assert!(registry.has_tool("web_search"));
        assert!(!registry.has_tool("unknown_tool"));
        assert_eq!(registry.tool_schemas().len(), 1);
    }

    #[test]
    fn registry_schemas_default_to_concurrent_unsafe() {
        let registry = McpToolRegistry {
            tool_to_server: HashMap::from([(
                "web_search".to_string(),
                "search-server".to_string(),
            )]),
            peers: HashMap::new(),
            tool_schemas: vec![serde_json::json!({
                "name": "web_search",
                "description": "search",
                "inputSchema": {"type": "object"},
                "concurrent_safe": false
            })],
            test_dispatch: None,
        };

        assert_eq!(
            registry.tool_schemas()[0]["concurrent_safe"],
            serde_json::Value::Bool(false)
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let registry = McpToolRegistry {
            tool_to_server: HashMap::new(),
            peers: HashMap::new(),
            tool_schemas: Vec::new(),
            test_dispatch: None,
        };
        let result = registry.call_tool("nonexistent", None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found in registry"));
    }

    impl McpToolRegistry {
        pub(crate) fn with_dispatch<I, F>(
            mappings: I,
            tool_schemas: Vec<serde_json::Value>,
            dispatch: F,
        ) -> Self
        where
            I: IntoIterator<Item = (String, String)>,
            F: Fn(
                    &str,
                    Option<serde_json::Map<String, serde_json::Value>>,
                ) -> Result<serde_json::Value, String>
                + Send
                + Sync
                + 'static,
        {
            Self {
                tool_to_server: mappings.into_iter().collect(),
                peers: HashMap::new(),
                tool_schemas,
                test_dispatch: Some(Arc::new(move |tool_name, arguments| {
                    let result = dispatch(tool_name, arguments);
                    Box::pin(async move { result })
                })),
            }
        }
    }

    #[tokio::test]
    async fn resolve_server_config_substitutes_env_and_credentials() {
        let app_state = test_context();
        let cred_repo = CredentialRepository::new(app_state.db.clone(), EventBus::noop());
        cred_repo
            .set("test", "TEST_TOKEN", "credential-secret")
            .await
            .expect("seed test credential");

        let unique = format!("DJINN_MCP_TEST_{}", uuid::Uuid::now_v7().simple());
        unsafe { std::env::set_var(&unique, "from-env") };

        let config = McpServerConfig {
            url: Some(format!("https://example.com/${{{unique}}}/mcp")),
            command: Some("ignored-command".to_string()),
            args: vec!["--flag".to_string()],
            env: HashMap::from([("API_KEY".to_string(), "${TEST_TOKEN}".to_string())]),
            headers: HashMap::from([(
                "Authorization".to_string(),
                "Bearer ${TEST_TOKEN}".to_string(),
            )]),
        };

        let resolved = resolve_server_config("example", &config, &app_state)
            .await
            .expect("resolve server config");

        assert_eq!(
            resolved.url.as_deref(),
            Some("https://example.com/from-env/mcp")
        );
        assert_eq!(resolved.command.as_deref(), Some("ignored-command"));
        assert_eq!(resolved.args, vec!["--flag"]);
        assert_eq!(
            resolved.env.get("API_KEY").map(String::as_str),
            Some("credential-secret")
        );
        assert_eq!(
            resolved.headers.get("Authorization").map(String::as_str),
            Some("Bearer credential-secret")
        );

        unsafe { std::env::remove_var(&unique) };
    }

    #[tokio::test]
    async fn resolve_server_config_errors_on_missing_placeholder() {
        let app_state = test_context();
        let config = McpServerConfig {
            url: Some("https://example.com/${MISSING_TOKEN}/mcp".to_string()),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            headers: HashMap::new(),
        };

        let error = resolve_server_config("example", &config, &app_state)
            .await
            .expect_err("missing placeholder should error");

        assert_eq!(error.variable, "MISSING_TOKEN");
        assert_eq!(error.field, "server `example` url");
    }

    #[test]
    fn resolved_transport_kind_is_explicit() {
        let http = ResolvedMcpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            headers: HashMap::new(),
        };
        let stdio = ResolvedMcpServerConfig {
            url: None,
            command: Some("server".to_string()),
            args: vec!["--stdio".to_string()],
            env: HashMap::from([("TOKEN".to_string(), "value".to_string())]),
            headers: HashMap::new(),
        };
        let unsupported = ResolvedMcpServerConfig {
            url: None,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            headers: HashMap::new(),
        };

        assert_eq!(http.transport_kind(), McpTransportKind::Http);
        assert_eq!(stdio.transport_kind(), McpTransportKind::Stdio);
        assert_eq!(unsupported.transport_kind(), McpTransportKind::Unsupported);
    }

    #[tokio::test]
    async fn connect_to_server_sends_resolved_headers() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let mut buffer = vec![0_u8; 8192];
            let size = stream.read(&mut buffer).await.expect("read request");
            let request = String::from_utf8_lossy(&buffer[..size]).to_string();
            let response = b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n";
            stream.write_all(response).await.expect("write response");
            request
        });

        let result = connect_to_server(
            &format!("http://{addr}/mcp"),
            &HashMap::from([(
                "Authorization".to_string(),
                "Bearer resolved-secret".to_string(),
            )]),
        )
        .await;

        assert!(result.is_err());
        let request = server.await.expect("server task result");
        assert!(request.contains("authorization: Bearer resolved-secret"));
    }

    #[tokio::test]
    async fn connect_and_discover_empty_servers() {
        let app_state = test_context();
        let result = connect_and_discover("test", "worker", &[], &app_state).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn connect_and_discover_skips_stdio_only() {
        let app_state = test_context();
        let servers = vec![(
            "stdio-server".to_string(),
            McpServerConfig {
                url: None,
                command: Some("my-server".to_string()),
                args: Vec::new(),
                env: HashMap::new(),
                headers: HashMap::new(),
            },
        )];
        let result = connect_and_discover("test", "worker", &servers, &app_state).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn connect_and_discover_skips_unreachable() {
        let app_state = test_context();
        let servers = vec![(
            "bad-server".to_string(),
            McpServerConfig {
                url: Some("http://127.0.0.1:1/mcp".to_string()),
                command: None,
                args: Vec::new(),
                env: HashMap::new(),
                headers: HashMap::new(),
            },
        )];
        let result = connect_and_discover("test", "worker", &servers, &app_state).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn connect_and_discover_skips_missing_placeholder_server() {
        let app_state = test_context();
        let servers = vec![(
            "missing-placeholder".to_string(),
            McpServerConfig {
                url: Some("https://example.com/${MISSING_VALUE}/mcp".to_string()),
                command: None,
                args: Vec::new(),
                env: HashMap::new(),
                headers: HashMap::new(),
            },
        )];

        let result = connect_and_discover("test", "worker", &servers, &app_state).await;
        assert!(result.is_none());
    }
}
