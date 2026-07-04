//! MCP client support for specialist agent sessions.
//!
//! Connects to resolved MCP servers at session start, discovers their tool
//! definitions via `tools/list`, and provides dispatch for tool calls routed
//! to those servers during the reply loop.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, RwLock};

#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use std::pin::Pin;

use regex::Regex;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool as RmcpTool};
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};

use crate::context::AgentContext;
use crate::extension::shared_schemas;
use crate::mcp_settings::McpServerConfig;
use djinn_provider::repos::CredentialRepository;

static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z0-9_]+)\}").expect("valid MCP placeholder regex"));

/// Maximum length of the advertised provider-facing MCP namespaced tool name,
/// including the `mcp__` prefix and both `__` separators.
pub const MCP_NAMESPACED_NAME_MAX_LEN: usize = 64;

/// Format a provider-facing MCP-namespaced tool name: `mcp__{server}__{tool}`.
///
/// Sanitizes both `server_name` and `tool_name` to `[A-Za-z0-9_-]` and bounds the
/// final name to [`MCP_NAMESPACED_NAME_MAX_LEN`] characters. If truncation is
/// required, the function preserves a deterministic prefix from each segment plus
/// the separators, so the advertised name remains readable and stable.
///
/// This is the name seen by the provider API; the remote server's original tool
/// name is preserved in `namespaced_to_original` and used at dispatch time.
pub fn mcp_namespaced_name(server_name: &str, tool_name: &str) -> String {
    let prefix = "mcp__";
    let separator = "__";
    let server = sanitize_mcp_name_segment(server_name);
    let tool = sanitize_mcp_name_segment(tool_name);
    let name = format!("{prefix}{server}{separator}{tool}");
    if name.len() <= MCP_NAMESPACED_NAME_MAX_LEN {
        return name;
    }
    truncate_mcp_namespaced_name(prefix, separator, &server, &tool)
}

fn sanitize_mcp_name_segment(segment: &str) -> String {
    if segment.is_empty() {
        return "_".to_string();
    }
    segment
        .chars()
        .map(|c| if is_mcp_name_safe_char(c) { c } else { '_' })
        .collect()
}

fn is_mcp_name_safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn truncate_mcp_namespaced_name(prefix: &str, separator: &str, server: &str, tool: &str) -> String {
    let budget = MCP_NAMESPACED_NAME_MAX_LEN.saturating_sub(prefix.len() + separator.len());
    // Allocate at least one character for each segment so neither is erased.
    let server_len = server.len().min(budget.saturating_sub(1));
    let remaining = budget.saturating_sub(server_len);
    let tool_len = tool.len().min(remaining.max(1));

    let server_prefix = &server[..server_len];
    let tool_prefix = &tool[..tool_len];
    format!("{prefix}{server_prefix}{separator}{tool_prefix}")
}

fn external_tool_schema_json(
    tool: &RmcpTool,
    namespaced: &str,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(tool)?;
    // Rewrite the tool name in the schema to the namespaced form while leaving
    // upstream MCP `annotations` intact.
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "name".to_string(),
            serde_json::Value::String(namespaced.to_string()),
        );
    }
    annotate_external_tool_schema_safety(&mut value);
    Ok(value)
}

fn annotation_bool(
    annotations: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
) -> Option<bool> {
    annotations?.get(key).and_then(serde_json::Value::as_bool)
}

fn annotate_external_tool_schema_safety(value: &mut serde_json::Value) {
    let annotations = value
        .get("annotations")
        .and_then(serde_json::Value::as_object);

    shared_schemas::annotate_tool_safety(
        value,
        shared_schemas::ToolSafetyAnnotations::new(
            annotation_bool(annotations, "readOnlyHint").unwrap_or(false),
            annotation_bool(annotations, "destructiveHint").unwrap_or(true),
            annotation_bool(annotations, "idempotentHint").unwrap_or(false),
            annotation_bool(annotations, "openWorldHint").unwrap_or(false),
            false,
        ),
    );
}

/// Interior-mutable routing state for [`McpToolRegistry`].
///
/// Wrapped in `Arc<RwLock<_>>` so the registry is `Clone`-safe and supports
/// notification-driven refresh without requiring `&mut self`.
struct RoutingState {
    /// namespaced_tool_name → server_name
    tool_to_server: HashMap<String, String>,
    /// namespaced_tool_name → original tool name (as the MCP server knows it)
    namespaced_to_original: HashMap<String, String>,
    /// server_name → live peer handle
    peers: HashMap<String, Arc<Peer<RoleClient>>>,
    /// Tools that were advertised at session start but have since been removed
    /// by a `tools/list_changed` refresh. These remain in the routing maps
    /// (so `has_tool` returns `true`) but `call_tool` returns a deterministic
    /// error instead of dispatching.
    unavailable: HashSet<String>,
}

/// Registry of MCP tool names → server connections built at session start.
///
/// Holds live `Peer<RoleClient>` handles and the tool-name→server-name mapping
/// so the reply loop can route unknown tool calls to the correct MCP server.
///
/// Tools are registered under namespaced names (`mcp__{server}__{tool}`) to
/// prevent collisions and make provenance visible in traces.
///
/// Routing state is interior-mutable via `Arc<RwLock<_>>` to support
/// notification-driven refreshes (`tools/list_changed`). The advertised
/// schema vector (`tool_schemas`) is session-fixed and never changes.
#[derive(Clone)]
pub struct McpToolRegistry {
    routing: Arc<RwLock<RoutingState>>,
    /// All discovered tool schemas ready to append to the session tool list.
    /// Session-fixed: once set at construction, never mutated.
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
    startup_timeout_ms: u64,
    request_timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpTransportKind {
    Http,
    Stdio,
    Unsupported,
}

#[allow(dead_code)]
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

    fn startup_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.startup_timeout_ms)
    }

    fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.request_timeout_ms)
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
    ///
    /// Returns `true` even for tools that have been marked unavailable by a
    /// server refresh — the name is still known, but `call_tool` will return
    /// a deterministic error.
    pub fn has_tool(&self, name: &str) -> bool {
        self.routing
            .read()
            .unwrap()
            .tool_to_server
            .contains_key(name)
    }

    /// Returns the MCP server name that provides the given tool, if any.
    pub fn server_for_tool(&self, name: &str) -> Option<String> {
        self.routing
            .read()
            .unwrap()
            .tool_to_server
            .get(name)
            .cloned()
    }

    /// Returns the discovered tool schemas (provider-compatible JSON).
    ///
    /// This list is session-fixed: it reflects the tools discovered at session
    /// start and is never updated by `tools/list_changed` refreshes.
    pub fn tool_schemas(&self) -> &[serde_json::Value] {
        &self.tool_schemas
    }

    /// Returns true if the given tool has been marked unavailable by a server
    /// refresh (i.e., the server no longer advertises it).
    pub fn is_unavailable(&self, name: &str) -> bool {
        self.routing.read().unwrap().unavailable.contains(name)
    }

    /// Dispatch a tool call to the MCP server that owns the given tool name.
    ///
    /// `tool_name` should be the namespaced name (e.g. `mcp__Tavilly__web_search`).
    /// The original tool name is resolved internally for the server call.
    ///
    /// Returns `Ok(json)` on success or `Err(message)` on failure. If the tool
    /// has been marked unavailable by a server refresh, returns a deterministic
    /// error without attempting to dispatch.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value, String> {
        #[cfg(test)]
        if let Some(dispatch) = &self.test_dispatch {
            return dispatch(tool_name, arguments).await;
        }

        // Snapshot the routing state under the read lock, then release before
        // the async peer.call_tool() so we never hold a std::sync::RwLock
        // across an await point.
        let (server_name, original_name, peer) = {
            let routing = self.routing.read().unwrap();

            if routing.unavailable.contains(tool_name) {
                return Err(format!(
                    "MCP tool `{tool_name}` is no longer available: \
                     removed by server refresh"
                ));
            }

            let server_name = routing
                .tool_to_server
                .get(tool_name)
                .ok_or_else(|| format!("MCP tool `{tool_name}` not found in registry"))?
                .clone();

            let original_name = routing
                .namespaced_to_original
                .get(tool_name)
                .cloned()
                .unwrap_or_else(|| tool_name.to_string());

            let peer = routing
                .peers
                .get(server_name.as_str())
                .ok_or_else(|| format!("MCP server `{server_name}` peer not found"))?
                .clone();

            (server_name, original_name, peer)
        };
        // Lock released — safe to await.

        // CallToolRequestParams is #[non_exhaustive] in rmcp 1.x; build via
        // new() + field assignment (meta/task default to None).
        let mut params = CallToolRequestParams::new(original_name);
        params.arguments = arguments.map(|m| m.into_iter().collect());

        let result = peer.call_tool(params).await.map_err(|e| {
            format!("MCP tool call `{tool_name}` on server `{server_name}` failed: {e}")
        })?;

        call_tool_result_to_json(result)
    }

    /// Apply a `tools/list_changed` refresh for a single server.
    ///
    /// `current_tool_names` contains the original (non-namespaced) tool names
    /// that the server currently advertises. Any previously registered tool
    /// for this server whose original name is NOT in `current_tool_names` is
    /// marked unavailable. Unchanged tools remain dispatchable.
    ///
    /// Newly discovered tools are NOT added to `tool_schemas` (session-fixed).
    /// Returns the list of namespaced tool names that were marked unavailable.
    pub fn apply_tools_refresh(
        &self,
        server_name: &str,
        current_tool_names: &[String],
    ) -> Vec<String> {
        let current_set: HashSet<&str> = current_tool_names.iter().map(|s| s.as_str()).collect();
        let mut routing = self.routing.write().unwrap();
        let mut newly_unavailable = Vec::new();

        // Collect namespaced names that belong to this server and whose
        // original name is no longer advertised.
        let to_mark: Vec<String> = routing
            .tool_to_server
            .iter()
            .filter_map(|(namespaced, srv)| {
                if srv != server_name {
                    return None;
                }
                let original = routing
                    .namespaced_to_original
                    .get(namespaced)
                    .map(|s| s.as_str())
                    .unwrap_or(namespaced.as_str());
                if !current_set.contains(original) && !routing.unavailable.contains(namespaced) {
                    Some(namespaced.clone())
                } else {
                    None
                }
            })
            .collect();

        for name in to_mark {
            routing.unavailable.insert(name.clone());
            newly_unavailable.push(name);
        }

        newly_unavailable
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
pub async fn connect_and_discover(
    task_short_id: &str,
    role_name: &str,
    servers: &[(String, McpServerConfig)],
    app_state: &AgentContext,
) -> Option<McpToolRegistry> {
    if servers.is_empty() {
        return None;
    }

    let mut tool_to_server: HashMap<String, String> = HashMap::new();
    let mut namespaced_to_original: HashMap<String, String> = HashMap::new();
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
                    let original_name = tool.name.to_string();
                    let namespaced = mcp_namespaced_name(name, &original_name);

                    // Convert rmcp Tool to provider-compatible JSON schema.
                    let schema = match external_tool_schema_json(&tool, &namespaced) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(
                                task_id = %task_short_id,
                                server = %name,
                                tool = %original_name,
                                error = %e,
                                "Failed to serialize MCP tool schema; skipping tool"
                            );
                            continue;
                        }
                    };

                    if tool_to_server.contains_key(&namespaced) {
                        tracing::warn!(
                            task_id = %task_short_id,
                            server = %name,
                            namespaced_tool = %namespaced,
                            original_tool = %original_name,
                            prior_server = %tool_to_server.get(&namespaced).unwrap_or(&"unknown".to_string()),
                            "Sanitized MCP tool name collides with an existing name; later server wins"
                        );
                    }

                    tool_to_server.insert(namespaced.clone(), name.clone());
                    namespaced_to_original.insert(namespaced, original_name);
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
        routing: Arc::new(RwLock::new(RoutingState {
            tool_to_server,
            namespaced_to_original,
            peers,
            unavailable: HashSet::new(),
        })),
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
        startup_timeout_ms: config.startup_timeout_ms,
        request_timeout_ms: config.request_timeout_ms,
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
    fn namespaced_name_matches_expected_format() {
        let name = mcp_namespaced_name("My Server", "tool.name!");
        assert!(name.starts_with("mcp__"));
        assert!(name.len() <= MCP_NAMESPACED_NAME_MAX_LEN);
        assert_regex_match(&name);
    }

    #[test]
    fn namespaced_name_sanitizes_invalid_characters() {
        assert_eq!(
            mcp_namespaced_name("server@foo", "tool/bar.baz"),
            "mcp__server_foo__tool_bar_baz"
        );
        assert_eq!(mcp_namespaced_name("a b", "c"), "mcp__a_b__c");
        assert_eq!(mcp_namespaced_name("a", "b c"), "mcp__a__b_c");
    }

    #[test]
    fn namespaced_name_preserves_alphanumeric_underscore_dash() {
        assert_eq!(
            mcp_namespaced_name("A-z_0-9", "tool-1_2"),
            "mcp__A-z_0-9__tool-1_2"
        );
    }

    #[test]
    fn namespaced_name_handles_empty_segments() {
        assert_eq!(mcp_namespaced_name("", "tool"), "mcp_____tool");
        assert_eq!(mcp_namespaced_name("server", ""), "mcp__server___");
    }

    #[test]
    fn namespaced_name_truncates_overlong_names() {
        let server = "a".repeat(100);
        let tool = "b".repeat(100);
        let name = mcp_namespaced_name(&server, &tool);
        assert!(name.starts_with("mcp__"));
        assert!(name.ends_with("__b"));
        assert_eq!(name.len(), MCP_NAMESPACED_NAME_MAX_LEN);
        assert_regex_match(&name);
    }

    #[test]
    fn namespaced_name_truncation_is_deterministic() {
        let server = "x".repeat(200);
        let tool = "y".repeat(200);
        let first = mcp_namespaced_name(&server, &tool);
        let second = mcp_namespaced_name(&server, &tool);
        assert_eq!(first, second);
    }

    #[test]
    fn namespaced_name_truncation_preserves_both_segments() {
        let server = "server".repeat(20);
        let tool = "tool".repeat(30);
        let name = mcp_namespaced_name(&server, &tool);
        assert!(name.starts_with("mcp__serverserver"));
        assert!(name.contains("__t"));
        assert_eq!(name.len(), MCP_NAMESPACED_NAME_MAX_LEN);
    }

    fn assert_regex_match(name: &str) {
        let re = regex::Regex::new("^mcp__[A-Za-z0-9_-]+__[A-Za-z0-9_-]+$").expect("valid regex");
        assert!(
            re.is_match(name),
            "name `{name}` does not match the expected pattern"
        );
    }

    #[test]
    fn call_tool_result_text_content() {
        use rmcp::model::Content;

        let result = CallToolResult::success(vec![Content::text("hello world")]);
        let json = call_tool_result_to_json(result).unwrap();
        assert_eq!(json, serde_json::json!({ "result": "hello world" }));
    }

    #[test]
    fn call_tool_result_json_content() {
        use rmcp::model::Content;

        let result = CallToolResult::success(vec![Content::text(r#"{"key": "value"}"#)]);
        let json = call_tool_result_to_json(result).unwrap();
        assert_eq!(json, serde_json::json!({ "key": "value" }));
    }

    #[test]
    fn call_tool_result_error() {
        use rmcp::model::Content;

        let result = CallToolResult::error(vec![Content::text("something went wrong")]);
        let err = call_tool_result_to_json(result).unwrap_err();
        assert_eq!(err, "something went wrong");
    }

    /// Helper: build an `Arc<RwLock<RoutingState>>` from raw maps.
    fn make_routing(
        tool_to_server: HashMap<String, String>,
        namespaced_to_original: HashMap<String, String>,
    ) -> Arc<RwLock<RoutingState>> {
        Arc::new(RwLock::new(RoutingState {
            tool_to_server,
            namespaced_to_original,
            peers: HashMap::new(),
            unavailable: HashSet::new(),
        }))
    }

    #[test]
    fn empty_registry_has_no_tools() {
        let registry = McpToolRegistry {
            routing: make_routing(HashMap::new(), HashMap::new()),
            tool_schemas: Vec::new(),
            test_dispatch: None,
        };
        assert!(!registry.has_tool("anything"));
        assert!(registry.tool_schemas().is_empty());
    }

    #[test]
    fn registry_lookup() {
        let namespaced = mcp_namespaced_name("search-server", "web_search");
        let mut tool_to_server = HashMap::new();
        tool_to_server.insert(namespaced.clone(), "search-server".to_string());
        let mut namespaced_to_original = HashMap::new();
        namespaced_to_original.insert(namespaced.clone(), "web_search".to_string());

        let registry = McpToolRegistry {
            routing: make_routing(tool_to_server, namespaced_to_original),
            tool_schemas: vec![serde_json::json!({"name": namespaced})],
            test_dispatch: None,
        };
        assert!(registry.has_tool(&mcp_namespaced_name("search-server", "web_search")));
        assert!(!registry.has_tool("web_search")); // raw name no longer works
        assert!(!registry.has_tool("unknown_tool"));
        assert_eq!(registry.tool_schemas().len(), 1);
    }

    #[test]
    fn registry_schemas_default_to_concurrent_unsafe() {
        let namespaced = mcp_namespaced_name("search-server", "web_search");
        let registry = McpToolRegistry {
            routing: make_routing(
                HashMap::from([(namespaced.clone(), "search-server".to_string())]),
                HashMap::from([(namespaced.clone(), "web_search".to_string())]),
            ),
            tool_schemas: vec![serde_json::json!({
                "name": namespaced,
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

    #[test]
    fn external_tool_schema_preserves_supplied_safety_annotations() {
        use rmcp::model::ToolAnnotations;
        use rmcp::object;

        let namespaced = mcp_namespaced_name("search-server", "web_search");
        let tool = RmcpTool::new(
            "web_search".to_string(),
            "search".to_string(),
            object!({"type": "object"}),
        )
        .annotate(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(true),
        );

        let schema = external_tool_schema_json(&tool, &namespaced).expect("serialize tool schema");

        assert_eq!(schema["name"], serde_json::Value::String(namespaced));
        assert_eq!(schema["annotations"]["readOnlyHint"], true);
        assert_eq!(schema["annotations"]["destructiveHint"], false);
        assert_eq!(schema["annotations"]["idempotentHint"], true);
        assert_eq!(schema["annotations"]["openWorldHint"], true);
        assert_eq!(schema["readOnly"], true);
        assert_eq!(schema["destructive"], false);
        assert_eq!(schema["idempotent"], true);
        assert_eq!(schema["openWorld"], true);
        assert_eq!(schema["concurrent_safe"], false);
    }

    #[test]
    fn external_tool_schema_without_annotations_defaults_fail_closed() {
        use rmcp::object;

        let namespaced = mcp_namespaced_name("unknown-server", "mutate");
        let tool = RmcpTool::new(
            "mutate".to_string(),
            "unknown third-party tool".to_string(),
            object!({"type": "object"}),
        );

        let schema = external_tool_schema_json(&tool, &namespaced).expect("serialize tool schema");

        assert_eq!(schema["name"], serde_json::Value::String(namespaced));
        assert!(schema.get("annotations").is_none());
        assert_eq!(schema["readOnly"], false);
        assert_eq!(schema["destructive"], true);
        assert_eq!(schema["idempotent"], false);
        assert_eq!(schema["openWorld"], false);
        assert_eq!(schema["concurrent_safe"], false);
    }

    #[tokio::test]
    async fn dispatch_routes_to_original_tool_name() {
        let original_tool = "remote/tool.name";
        let advertised = mcp_namespaced_name("my-server", original_tool);
        let mut tool_to_server = HashMap::new();
        tool_to_server.insert(advertised.clone(), "my-server".to_string());
        let mut namespaced_to_original = HashMap::new();
        namespaced_to_original.insert(advertised.clone(), original_tool.to_string());

        let received = std::sync::Arc::new(std::sync::Mutex::new(None));
        let received_clone = received.clone();
        let registry = McpToolRegistry {
            routing: make_routing(tool_to_server, namespaced_to_original),
            tool_schemas: Vec::new(),
            test_dispatch: Some(Arc::new(move |_tool_name, _arguments| {
                let received = received_clone.clone();
                Box::pin(async move {
                    *received.lock().unwrap() = Some(original_tool.to_string());
                    Ok(serde_json::json!({ "tool": original_tool }))
                })
            })),
        };

        let result = registry.call_tool(&advertised, None).await;
        assert!(result.is_ok());
        assert_eq!(received.lock().unwrap().as_deref(), Some(original_tool));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let registry = McpToolRegistry {
            routing: make_routing(HashMap::new(), HashMap::new()),
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
            let tool_to_server: HashMap<String, String> = mappings.into_iter().collect();
            let namespaced_to_original = HashMap::new(); // test dispatch handles names directly
            Self {
                routing: make_routing(tool_to_server, namespaced_to_original),
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
            ..Default::default()
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
        assert_eq!(resolved.startup_timeout_ms, 30_000);
        assert_eq!(resolved.request_timeout_ms, 120_000);

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
            ..Default::default()
        };

        let error = resolve_server_config("example", &config, &app_state)
            .await
            .expect_err("missing placeholder should error");

        assert_eq!(error.variable, "MISSING_TOKEN");
        assert_eq!(error.field, "server `example` url");
    }

    #[tokio::test]
    async fn resolve_server_config_reports_missing_header_placeholder() {
        let app_state = test_context();
        let config = McpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            headers: HashMap::from([(
                "Authorization".to_string(),
                "Bearer ${MISSING_HEADER_TOKEN}".to_string(),
            )]),
            ..Default::default()
        };

        let error = resolve_server_config("example", &config, &app_state)
            .await
            .expect_err("missing header placeholder should error");

        assert_eq!(error.variable, "MISSING_HEADER_TOKEN");
        assert_eq!(error.field, "server `example` header `Authorization`");
    }

    #[test]
    fn resolved_transport_kind_is_explicit() {
        let http = ResolvedMcpServerConfig {
            url: Some("https://example.com/mcp".to_string()),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            headers: HashMap::new(),
            startup_timeout_ms: 30_000,
            request_timeout_ms: 120_000,
        };
        let stdio = ResolvedMcpServerConfig {
            url: None,
            command: Some("server".to_string()),
            args: vec!["--stdio".to_string()],
            env: HashMap::from([("TOKEN".to_string(), "value".to_string())]),
            headers: HashMap::new(),
            startup_timeout_ms: 30_000,
            request_timeout_ms: 120_000,
        };
        let unsupported = ResolvedMcpServerConfig {
            url: None,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            headers: HashMap::new(),
            startup_timeout_ms: 30_000,
            request_timeout_ms: 120_000,
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
                ..Default::default()
            },
        )];
        let result = connect_and_discover("test", "worker", &servers, &app_state).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn connect_and_discover_skips_unsupported_transport() {
        let app_state = test_context();
        let servers = vec![(
            "unsupported-server".to_string(),
            McpServerConfig {
                url: None,
                command: None,
                args: vec!["--unused".to_string()],
                env: HashMap::from([("TOKEN".to_string(), "value".to_string())]),
                headers: HashMap::from([("Authorization".to_string(), "Bearer token".to_string())]),
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            },
        )];

        let result = connect_and_discover("test", "worker", &servers, &app_state).await;
        assert!(result.is_none());
    }

    // ── Refresh / mutability tests ──────────────────────────────────────

    #[tokio::test]
    async fn removed_advertised_tool_returns_deterministic_error() {
        let namespaced = mcp_namespaced_name("my-server", "web_search");
        let mut tool_to_server = HashMap::new();
        tool_to_server.insert(namespaced.clone(), "my-server".to_string());
        let mut namespaced_to_original = HashMap::new();
        namespaced_to_original.insert(namespaced.clone(), "web_search".to_string());

        let registry = McpToolRegistry {
            routing: make_routing(tool_to_server, namespaced_to_original),
            tool_schemas: vec![serde_json::json!({"name": namespaced})],
            test_dispatch: None,
        };

        // Tool is available initially.
        assert!(registry.has_tool(&namespaced));
        assert!(!registry.is_unavailable(&namespaced));

        // Simulate server refresh that no longer advertises web_search.
        let removed = registry.apply_tools_refresh("my-server", &[]);
        assert_eq!(removed, vec![namespaced.clone()]);
        assert!(registry.is_unavailable(&namespaced));
        // has_tool still returns true — the name is known.
        assert!(registry.has_tool(&namespaced));

        // call_tool returns a deterministic error.
        let result = registry.call_tool(&namespaced, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("no longer available"),
            "expected deterministic unavailable error, got: {err}"
        );
        assert!(
            err.contains("removed by server refresh"),
            "error should mention server refresh, got: {err}"
        );
    }

    #[test]
    fn refresh_does_not_add_newly_discovered_tools_to_schemas() {
        let namespaced = mcp_namespaced_name("my-server", "existing_tool");
        let mut tool_to_server = HashMap::new();
        tool_to_server.insert(namespaced.clone(), "my-server".to_string());
        let mut namespaced_to_original = HashMap::new();
        namespaced_to_original.insert(namespaced.clone(), "existing_tool".to_string());

        let registry = McpToolRegistry {
            routing: make_routing(tool_to_server, namespaced_to_original),
            tool_schemas: vec![serde_json::json!({"name": namespaced})],
            test_dispatch: None,
        };

        let schemas_before = registry.tool_schemas().len();

        // Apply refresh — the server now advertises a brand new tool as well.
        registry.apply_tools_refresh(
            "my-server",
            &["existing_tool".to_string(), "new_tool".to_string()],
        );

        // tool_schemas is session-fixed: still the same length.
        assert_eq!(
            registry.tool_schemas().len(),
            schemas_before,
            "tool_schemas must not grow after a refresh"
        );
    }

    #[tokio::test]
    async fn unchanged_advertised_tool_remains_dispatchable_after_refresh() {
        let namespaced = mcp_namespaced_name("my-server", "stable_tool");
        let mut tool_to_server = HashMap::new();
        tool_to_server.insert(namespaced.clone(), "my-server".to_string());
        let mut namespaced_to_original = HashMap::new();
        namespaced_to_original.insert(namespaced.clone(), "stable_tool".to_string());

        // Use test_dispatch to verify the tool is still callable.
        let registry = McpToolRegistry {
            routing: make_routing(tool_to_server, namespaced_to_original),
            tool_schemas: vec![serde_json::json!({"name": namespaced})],
            test_dispatch: Some(Arc::new(|_, _| {
                Box::pin(async { Ok(serde_json::json!({"ok": true})) })
            })),
        };

        // Refresh: stable_tool is still advertised.
        let removed = registry.apply_tools_refresh("my-server", &["stable_tool".to_string()]);
        assert!(removed.is_empty(), "no tools should be removed");
        assert!(!registry.is_unavailable(&namespaced));

        // Tool is still dispatchable.
        let result = registry.call_tool(&namespaced, None).await;
        assert!(result.is_ok(), "unchanged tool should still dispatch");
    }

    #[tokio::test]
    async fn routing_state_is_clone_safe() {
        let namespaced = mcp_namespaced_name("my-server", "shared_tool");
        let mut tool_to_server = HashMap::new();
        tool_to_server.insert(namespaced.clone(), "my-server".to_string());
        let mut namespaced_to_original = HashMap::new();
        namespaced_to_original.insert(namespaced.clone(), "shared_tool".to_string());

        let registry = McpToolRegistry {
            routing: make_routing(tool_to_server, namespaced_to_original),
            tool_schemas: vec![serde_json::json!({"name": namespaced})],
            test_dispatch: None,
        };

        // Clone shares the same routing state.
        let clone = registry.clone();
        assert!(clone.has_tool(&namespaced));

        // Mutating via the original is visible to the clone.
        registry.apply_tools_refresh("my-server", &[]);
        assert!(
            clone.is_unavailable(&namespaced),
            "clone must see unavailability set on original"
        );
    }
}
