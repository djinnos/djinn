//! MCP client support for specialist agent sessions.
//!
//! Connects to resolved MCP servers at session start, discovers their tool
//! definitions via `tools/list`, and provides dispatch for tool calls routed
//! to those servers during the reply loop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

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
    /// Per-server request timeout in milliseconds, populated at discovery time
    /// from each server's `McpServerConfig::request_timeout_ms`.
    request_timeouts: HashMap<String, u64>,
    /// Server instructions captured at initialization from each successfully
    /// connected server. Stored in deterministic server-name order; empty or
    /// whitespace-only instructions are omitted.
    #[allow(dead_code)] // Mirrored on McpToolRegistry; stored here for future refresh use.
    server_instructions: BTreeMap<String, String>,
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
    /// Server instructions captured at initialization, in deterministic order.
    /// Mirrors the `RoutingState` value so consumers can read it without
    /// acquiring the lock.
    server_instructions: BTreeMap<String, String>,
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

    /// Returns the non-empty server instructions captured at initialization,
    /// in server-name-sorted order. Failed servers and servers that returned no
    /// instructions are omitted.
    pub fn server_instructions(&self) -> &BTreeMap<String, String> {
        &self.server_instructions
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
        // Snapshot the routing state under the read lock, then release before
        // the async peer.call_tool() so we never hold a std::sync::RwLock
        // across an await point.
        let (server_name, original_name, peer, timeout_ms) = {
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

            let timeout_ms = routing
                .request_timeouts
                .get(server_name.as_str())
                .copied()
                .unwrap_or(McpServerConfig::default_request_timeout_ms());

            let peer = routing.peers.get(server_name.as_str()).cloned();

            (server_name, original_name, peer, timeout_ms)
        };
        // Lock released — safe to await.

        let timeout_duration = Duration::from_millis(timeout_ms);

        // In test mode with a dispatch function, use the callback directly
        // wrapped in the request timeout.
        #[cfg(test)]
        if let Some(dispatch) = &self.test_dispatch {
            let dispatch = dispatch.clone();
            let tool = tool_name.to_string();
            let tn = server_name.clone();
            return match tokio::time::timeout(timeout_duration, dispatch(&tool, arguments)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(format!(
                    "MCP tool call `{tool_name}` on server `{tn}` timed out \
                     after {timeout_ms}ms"
                )),
            };
        }

        let peer = peer.ok_or_else(|| format!("MCP server `{server_name}` peer not found"))?;

        // CallToolRequestParams is #[non_exhaustive] in rmcp 1.x; build via
        // new() + field assignment (meta/task default to None).
        let mut params = CallToolRequestParams::new(original_name);
        params.arguments = arguments.map(|m| m.into_iter().collect());

        match tokio::time::timeout(timeout_duration, peer.call_tool(params)).await {
            Ok(Ok(result)) => call_tool_result_to_json(result),
            Ok(Err(e)) => Err(format!(
                "MCP tool call `{tool_name}` on server `{server_name}` failed: {e}"
            )),
            Err(_elapsed) => Err(format!(
                "MCP tool call `{tool_name}` on server `{server_name}` timed out \
                     after {timeout_ms}ms"
            )),
        }
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
    let mut request_timeouts: HashMap<String, u64> = HashMap::new();
    let mut tool_schemas: Vec<serde_json::Value> = Vec::new();
    let mut server_instructions: BTreeMap<String, String> = BTreeMap::new();

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

        // Connect to the MCP server and discover tools within the startup timeout.
        // The combined connect + initialize + initial tools/list must complete
        // within `startup_timeout_ms`; nonresponsive servers are logged and
        // skipped without failing the whole discovery.
        let startup_duration = resolved.startup_timeout();
        let startup_result =
            tokio::time::timeout(startup_duration, startup_and_list(&url, &resolved.headers)).await;

        let (peer, list_result) = match startup_result {
            Ok(Ok((peer, list_result))) => {
                tracing::info!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    url = %url,
                    header_count = resolved.headers.len(),
                    "Connected to MCP server"
                );
                (Arc::new(peer), list_result)
            }
            Ok(Err(e)) => {
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
            Err(_elapsed) => {
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    url = %url,
                    startup_timeout_ms = resolved.startup_timeout_ms,
                    "MCP server startup timed out (connect + initialize + tools/list); skipping"
                );
                continue;
            }
        };

        // Log server capabilities captured during initialization, if available.
        // `Peer<RoleClient>::peer_info()` returns the `InitializeResult`
        // which carries `instructions`, `capabilities`, and server metadata.
        // Instructions and resource tools are NOT exposed here — those are
        // owned by sibling epics (yjc6 / hyeu).
        if let Some(info) = peer.peer_info() {
            tracing::debug!(
                task_id = %task_short_id,
                server = %name,
                has_instructions = info.instructions.is_some(),
                has_tools_capability = info.capabilities.tools.is_some(),
                has_resources_capability = info.capabilities.resources.is_some(),
                has_prompts_capability = info.capabilities.prompts.is_some(),
                has_logging_capability = info.capabilities.logging.is_some(),
                "MCP server initialize capabilities"
            );
            if let Some(instr) = info.instructions.as_deref() {
                let trimmed = instr.trim();
                if !trimmed.is_empty() {
                    server_instructions.insert(name.clone(), trimmed.to_string());
                }
            }
        }

        // Discover tools from this server.
        // The list_result is already a ListToolsResult — errors were handled
        // in the startup_and_list call above (mapped to Err which was caught
        // by the Ok(Err(e)) arm of the startup timeout match).
        {
            let result = list_result;
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
            request_timeouts.insert(name.clone(), resolved.request_timeout_ms);
            tracing::info!(
                task_id = %task_short_id,
                role = %role_name,
                server = %name,
                tool_count,
                "Discovered MCP tools"
            );
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
            request_timeouts,
            unavailable: HashSet::new(),
            server_instructions: server_instructions.clone(),
        })),
        tool_schemas,
        server_instructions,
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

/// Combined connect + initialize + initial `tools/list` in a single future.
///
/// This is the unit that [`connect_and_discover`] wraps with
/// `startup_timeout_ms`.  Breaking it out lets `tokio::time::timeout` cancel
/// the whole operation if either the transport handshake or the initial
/// tool enumeration stalls.
///
/// Returns the connected peer *and* the raw `ListToolsResult` so the caller
/// can inspect tool definitions without a second round-trip.
async fn startup_and_list(
    url: &str,
    headers: &HashMap<String, String>,
) -> Result<(Peer<RoleClient>, rmcp::model::ListToolsResult), String> {
    let peer = connect_to_server(url, headers).await?;
    let result = peer
        .list_tools(None)
        .await
        .map_err(|e| format!("MCP tools/list failed: {e}"))?;
    Ok((peer, result))
}

#[cfg(test)]
mod tests;
