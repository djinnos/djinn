//! MCP client support for specialist agent sessions.
//!
//! Connects to resolved MCP servers at session start, discovers their tool
//! definitions via `tools/list`, and provides dispatch for tool calls routed
//! to those servers during the reply loop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[cfg(test)]
use std::future::Future;
#[cfg(test)]
use std::pin::Pin;

use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ReadResourceRequestParams, Resource as RmcpResource,
    ResourceContents, Tool as RmcpTool,
};
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};

use crate::context::AgentContext;
use crate::extension::shared_schemas;
use crate::extension_diagnostics::ExtensionDiagnosticFact;
use crate::mcp_settings::McpServerConfig;
use djinn_core::extension_diagnostics::{
    ExtensionLoadPhase, ExtensionLoadRemedyCode, ExtensionLoadSeverity, ExtensionLoadSourceKind,
};

mod config;
use config::{McpTransportKind, resolve_server_config};

/// Maximum length of the advertised provider-facing MCP namespaced tool name,
/// including the `mcp__` prefix and both `__` separators.
pub const MCP_NAMESPACED_NAME_MAX_LEN: usize = 64;

/// Maximum size of a text MCP resource that will be rendered inline in the
/// tool result. Larger text resources and all binary resources are omitted with
/// a descriptive message to keep context bounded.
pub const MAX_MCP_RESOURCE_TEXT_BYTES: usize = 10 * 1024 * 1024;

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

// ── MCP notification handler ─────────────────────────────────────────

/// Compute a deterministic fingerprint for an MCP tool based on its
/// description and input schema.
///
/// The tool *name* is intentionally excluded so that a renamed tool
/// (same description + schema, different name) produces the same
/// fingerprint.  The fingerprint is used for rename detection during
/// `tools/list_changed` refreshes: if exactly one newly-advertised
/// tool shares a removed tool's fingerprint, the old wire alias is
/// re-routed to the new tool name.
fn compute_tool_fingerprint(tool: &RmcpTool) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Hash the description (Option<Cow<str>>).
    tool.description.hash(&mut hasher);
    // Hash the input schema as canonical JSON bytes.
    serde_json::to_string(&*tool.input_schema)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Maps an MCP [`LoggingLevel`] to the corresponding [`tracing::Level`].
///
/// The mapping follows syslog-to-tracing conventions:
/// - `Debug` → `TRACE` (MCP debug is the most verbose)
/// - `Info` → `DEBUG` (MCP info is informational noise)
/// - `Notice` → `INFO` (MCP notice is worth highlighting)
/// - `Warning` → `WARN`
/// - `Error` → `ERROR`
/// - `Critical` / `Alert` / `Emergency` → `ERROR` (tracing has no above-error)
fn mcp_log_level_to_tracing(level: rmcp::model::LoggingLevel) -> tracing::Level {
    match level {
        rmcp::model::LoggingLevel::Debug => tracing::Level::TRACE,
        rmcp::model::LoggingLevel::Info => tracing::Level::DEBUG,
        rmcp::model::LoggingLevel::Notice => tracing::Level::INFO,
        rmcp::model::LoggingLevel::Warning => tracing::Level::WARN,
        rmcp::model::LoggingLevel::Error
        | rmcp::model::LoggingLevel::Critical
        | rmcp::model::LoggingLevel::Alert
        | rmcp::model::LoggingLevel::Emergency => tracing::Level::ERROR,
    }
}

/// Extract a human-readable message string from a logging notification's
/// `data` field. Handles strings, null, and objects/arrays by falling
/// back to their JSON representation.
fn log_data_to_message(data: &serde_json::Value) -> String {
    match data {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "<null>".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| format!("{other:?}")),
    }
}

/// An rmcp `ClientHandler` that observes MCP notifications from connected
/// servers:
/// - `notifications/message` (logging) — emitted through host `tracing` with
///   structured `{server, logger, level, task_short_id}` fields.
/// - `notifications/tools/list_changed` — triggers a request-timeout-bounded
///   `tools/list` refresh for the notifying server without holding registry
///   write locks across the network await.
#[derive(Clone)]
struct McpNotificationHandler {
    server_name: String,
    task_short_id: String,
    routing: Arc<RwLock<RoutingState>>,
}

impl rmcp::ClientHandler for McpNotificationHandler {
    fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: rmcp::service::NotificationContext<rmcp::service::RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        // Extract all values into owned types before the tracing call so the
        // returned `ready` future does not borrow any locals that would be
        // dropped at function exit.
        let level = mcp_log_level_to_tracing(params.level);
        let logger = params.logger.unwrap_or_default();
        let msg = log_data_to_message(&params.data);
        let server = self.server_name.clone();
        let task = self.task_short_id.clone();
        let lvl = params.level; // LoggingLevel is Copy

        // tracing macros require a literal Level, so we dispatch explicitly.
        // Each structured field (server, logger, level, task_short_id) is
        // included as a named field on the tracing event. The tracing `target`
        // defaults to the module path; the `server` field identifies the MCP
        // server that produced the log message.
        match level {
            tracing::Level::TRACE => {
                tracing::trace!(
                    server = %server, logger = %logger,
                    level = ?lvl, task_short_id = %task,
                    "{msg}"
                );
            }
            tracing::Level::DEBUG => {
                tracing::debug!(
                    server = %server, logger = %logger,
                    level = ?lvl, task_short_id = %task,
                    "{msg}"
                );
            }
            tracing::Level::INFO => {
                tracing::info!(
                    server = %server, logger = %logger,
                    level = ?lvl, task_short_id = %task,
                    "{msg}"
                );
            }
            tracing::Level::WARN => {
                tracing::warn!(
                    server = %server, logger = %logger,
                    level = ?lvl, task_short_id = %task,
                    "{msg}"
                );
            }
            tracing::Level::ERROR => {
                tracing::error!(
                    server = %server, logger = %logger,
                    level = ?lvl, task_short_id = %task,
                    "{msg}"
                );
            }
        }

        std::future::ready(())
    }

    fn on_tool_list_changed(
        &self,
        context: rmcp::service::NotificationContext<rmcp::service::RoleClient>,
    ) -> impl std::future::Future<Output = ()> + Send + '_ {
        let server_name = self.server_name.clone();
        let task_short_id = self.task_short_id.clone();
        let routing = self.routing.clone();
        let peer = context.peer.clone();

        async move {
            tracing::info!(
                server = %server_name,
                task_short_id = %task_short_id,
                "Received tools/list_changed notification; refreshing tool list"
            );

            // Snapshot the request timeout under a read lock, then release.
            let timeout_ms = {
                let r = routing.read().unwrap();
                r.request_timeouts
                    .get(&server_name)
                    .copied()
                    .unwrap_or(McpServerConfig::default_request_timeout_ms())
            };
            let timeout = Duration::from_millis(timeout_ms);

            // Issue tools/list with request timeout — no lock held.
            let result = match tokio::time::timeout(timeout, peer.list_tools(None)).await {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => {
                    tracing::error!(
                        server = %server_name,
                        task_short_id = %task_short_id,
                        error = %e,
                        "tools/list refresh failed during tools/list_changed"
                    );
                    return;
                }
                Err(_elapsed) => {
                    tracing::error!(
                        server = %server_name,
                        task_short_id = %task_short_id,
                        timeout_ms = timeout_ms,
                        "tools/list refresh timed out during tools/list_changed"
                    );
                    return;
                }
            };

            // Apply the refresh under a write lock (brief — no network I/O).
            let (unavailable, renamed) = {
                let mut r = routing.write().unwrap();
                r.apply_tools_list_result(&server_name, &result.tools)
            };

            if !unavailable.is_empty() {
                tracing::warn!(
                    server = %server_name,
                    task_short_id = %task_short_id,
                    removed_count = unavailable.len(),
                    tools = ?unavailable,
                    "tools/list_changed: marked tools as no longer available"
                );
            }
            if !renamed.is_empty() {
                tracing::info!(
                    server = %server_name,
                    task_short_id = %task_short_id,
                    renamed_count = renamed.len(),
                    tools = ?renamed,
                    "tools/list_changed: detected tool renames"
                );
            }
        }
    }
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
    /// Schema fingerprints for each registered tool (namespaced_name → hash).
    /// Used for rename detection during `tools/list_changed` refreshes.
    tool_fingerprints: HashMap<String, u64>,
    /// Server names that advertised the resources capability at connect time.
    /// Only these servers may be targeted by `list_resources` / `read_resource`.
    resource_servers: HashSet<String>,
}

impl RoutingState {
    /// Apply a `tools/list_changed` refresh using the full tool list from
    /// the MCP server.
    ///
    /// Performs rename detection: if exactly one newly-advertised tool shares
    /// a removed tool's schema fingerprint (hash of description + input_schema),
    /// the old namespaced alias is re-routed to the new original tool name.
    /// Otherwise the removed tool is marked unavailable.
    ///
    /// Newly discovered tools are NOT registered (session-fixed schemas).
    ///
    /// Returns `(newly_unavailable, renamed)` where each vector contains the
    /// namespaced tool names affected.
    fn apply_tools_list_result(
        &mut self,
        server_name: &str,
        new_tools: &[RmcpTool],
    ) -> (Vec<String>, Vec<String>) {
        // Build the set of original names currently advertised.
        let current_names: HashSet<&str> = new_tools.iter().map(|t| t.name.as_ref()).collect();

        // Collect tools belonging to this server.
        let server_tools: Vec<(String, String)> = self
            .tool_to_server
            .iter()
            .filter_map(|(namespaced, srv)| {
                if srv != server_name {
                    return None;
                }
                let original = self
                    .namespaced_to_original
                    .get(namespaced)
                    .cloned()
                    .unwrap_or_else(|| namespaced.clone());
                Some((namespaced.clone(), original))
            })
            .collect();

        // Partition into unchanged vs. potentially-removed.
        let mut removed: Vec<(String, String, u64)> = Vec::new();
        let mut newly_unavailable = Vec::new();

        for (namespaced, original) in &server_tools {
            if current_names.contains(original.as_str()) {
                // Still advertised under the same name — unchanged.
                continue;
            }
            // Tool name is gone.  Check for rename by fingerprint.
            if let Some(&fp) = self.tool_fingerprints.get(namespaced) {
                removed.push((namespaced.clone(), original.clone(), fp));
            } else {
                // No fingerprint on record — treat as removed.
                self.unavailable.insert(namespaced.clone());
                newly_unavailable.push(namespaced.clone());
            }
        }

        // Build fingerprint → new tool names map for rename candidates.
        // Only consider tools that are genuinely *new* (not already registered).
        let mut fp_to_new: HashMap<u64, Vec<&str>> = HashMap::new();
        for tool in new_tools {
            let namespaced = mcp_namespaced_name(server_name, &tool.name);
            if !self.tool_to_server.contains_key(&namespaced) {
                let fp = compute_tool_fingerprint(tool);
                fp_to_new.entry(fp).or_default().push(tool.name.as_ref());
            }
        }

        let mut renamed = Vec::new();

        for (namespaced, _original, fp) in &removed {
            match fp_to_new.get(fp) {
                Some(candidates) if candidates.len() == 1 => {
                    // Unique fingerprint match — treat as rename.
                    let new_original = candidates[0];
                    self.namespaced_to_original
                        .insert(namespaced.clone(), new_original.to_string());
                    self.tool_fingerprints.insert(namespaced.clone(), *fp);
                    renamed.push(namespaced.clone());
                }
                _ => {
                    // Zero or ambiguous matches — treat as removed.
                    self.unavailable.insert(namespaced.clone());
                    newly_unavailable.push(namespaced.clone());
                }
            }
        }

        (newly_unavailable, renamed)
    }
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
    /// Server names that advertised the resources capability. Mirrors the
    /// `RoutingState` value so consumers can read it without acquiring the lock.
    resource_servers: Vec<String>,
    #[cfg(test)]
    test_dispatch: Option<Arc<TestDispatchFn>>,
}

#[cfg(test)]
type TestDispatchFuture = Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send>>;

#[cfg(test)]
type TestDispatchFn = dyn Fn(&str, Option<serde_json::Map<String, serde_json::Value>>) -> TestDispatchFuture
    + Send
    + Sync;

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

    /// Returns `true` if at least one connected server advertised the
    /// `resources` capability during discovery.
    pub fn has_resource_servers(&self) -> bool {
        !self.resource_servers.is_empty()
    }

    /// Returns the sorted names of connected servers that advertised the
    /// `resources` capability.  Deterministic order for stable gating logic.
    pub fn resource_server_names(&self) -> &[String] {
        &self.resource_servers
    }

    /// List resources from one or all resource-capable MCP servers.
    ///
    /// When `server` is `None`, resources from every resource-capable server
    /// are collected.  Failures for individual servers are logged and skipped;
    /// a deterministic `Err` is returned only when *every* requested server
    /// failed (or no server matched).
    ///
    /// Uses the same per-server request timeout policy as `call_tool`.
    /// Does not hold registry locks across network awaits.
    pub async fn list_resources(
        &self,
        server: Option<&str>,
    ) -> Result<Vec<(String, RmcpResource)>, String> {
        // Snapshot the relevant routing state under the read lock.
        let snapshot: Vec<(String, Arc<Peer<RoleClient>>, u64)> = {
            let routing = self.routing.read().unwrap();
            let target_servers: Vec<&str> = match server {
                Some(name) => {
                    if !routing.resource_servers.contains(name) {
                        return Err(format!(
                            "MCP server `{name}` is not resource-capable or not connected"
                        ));
                    }
                    vec![name]
                }
                None => routing
                    .resource_servers
                    .iter()
                    .map(String::as_str)
                    .collect(),
            };
            if target_servers.is_empty() {
                return Ok(Vec::new());
            }
            target_servers
                .into_iter()
                .filter_map(|name| {
                    let peer = routing.peers.get(name)?;
                    let timeout_ms = routing
                        .request_timeouts
                        .get(name)
                        .copied()
                        .unwrap_or(McpServerConfig::default_request_timeout_ms());
                    Some((name.to_string(), peer.clone(), timeout_ms))
                })
                .collect()
        };
        // Lock released — safe to await.

        if snapshot.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_resources: Vec<(String, RmcpResource)> = Vec::new();
        let mut had_success = false;

        for (server_name, peer, timeout_ms) in &snapshot {
            let timeout_duration = Duration::from_millis(*timeout_ms);
            match tokio::time::timeout(timeout_duration, peer.list_resources(None)).await {
                Ok(Ok(result)) => {
                    for resource in result.resources {
                        all_resources.push((server_name.clone(), resource));
                    }
                    had_success = true;
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        server = %server_name,
                        error = %e,
                        "MCP resources/list failed for server; skipping"
                    );
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        server = %server_name,
                        timeout_ms = *timeout_ms,
                        "MCP resources/list timed out for server; skipping"
                    );
                }
            }
        }

        if had_success || !all_resources.is_empty() {
            Ok(all_resources)
        } else {
            Err("All resource-capable MCP servers failed to list resources".to_string())
        }
    }

    /// Read a single resource from a specific MCP server by URI.
    ///
    /// Returns `Err` for missing server, timeout, or rmcp failure.
    /// Uses the same per-server request timeout policy as `call_tool`.
    /// Does not hold registry locks across network awaits.
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<Vec<ResourceContents>, String> {
        // Snapshot the routing state under the read lock.
        let (peer, timeout_ms) = {
            let routing = self.routing.read().unwrap();

            if !routing.resource_servers.contains(server) {
                return Err(format!(
                    "MCP server `{server}` is not resource-capable or not connected"
                ));
            }

            let timeout_ms = routing
                .request_timeouts
                .get(server)
                .copied()
                .unwrap_or(McpServerConfig::default_request_timeout_ms());

            let peer = routing
                .peers
                .get(server)
                .cloned()
                .ok_or_else(|| format!("MCP server `{server}` peer not found"))?;

            (peer, timeout_ms)
        };
        // Lock released — safe to await.

        let timeout_duration = Duration::from_millis(timeout_ms);
        let params = ReadResourceRequestParams::new(uri);

        match tokio::time::timeout(timeout_duration, peer.read_resource(params)).await {
            Ok(Ok(result)) => Ok(result.contents),
            Ok(Err(e)) => Err(format!(
                "MCP resources/read for `{uri}` on server `{server}` failed: {e}"
            )),
            Err(_elapsed) => Err(format!(
                "MCP resources/read for `{uri}` on server `{server}` timed out \
                 after {timeout_ms}ms"
            )),
        }
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

/// Additive result from MCP startup discovery.
///
/// Lifecycle owns load-attempt association and persistence. This result only
/// carries bounded facts to the shared diagnostic producer.
pub(crate) struct McpDiscoveryResult {
    pub registry: Option<McpToolRegistry>,
    #[expect(
        dead_code,
        reason = "lifecycle consumes diagnostics when it owns attempt persistence"
    )]
    pub diagnostics: Vec<ExtensionDiagnosticFact>,
}

/// The reachable startup boundaries exposed by this HTTP-only loader.
#[derive(Debug, Clone, Copy)]
enum McpStartupFailure {
    Transport,
    // rmcp serves connection and initialize at the same boundary.
    Handshake,
    ToolsList,
}

impl std::fmt::Display for McpStartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Transport => "MCP transport configuration failed",
            Self::Handshake => "MCP transport handshake failed",
            Self::ToolsList => "MCP tools/list failed",
        })
    }
}

impl McpStartupFailure {
    fn diagnostic(self, server_name: &str) -> ExtensionDiagnosticFact {
        let (phase, remedy_code, summary_material) = match self {
            Self::Transport => (
                ExtensionLoadPhase::Transport,
                ExtensionLoadRemedyCode::CheckTransport,
                "MCP transport configuration could not be initialized.",
            ),
            Self::Handshake => (
                ExtensionLoadPhase::Handshake,
                ExtensionLoadRemedyCode::CheckServer,
                "MCP connection or initialization failed.",
            ),
            Self::ToolsList => (
                ExtensionLoadPhase::ToolsList,
                ExtensionLoadRemedyCode::CheckServer,
                "Initial MCP tools/list request failed.",
            ),
        };
        mcp_diagnostic(server_name, phase, remedy_code, summary_material)
    }
}

fn mcp_diagnostic(
    server_name: &str,
    phase: ExtensionLoadPhase,
    remedy_code: ExtensionLoadRemedyCode,
    summary_material: &'static str,
) -> ExtensionDiagnosticFact {
    ExtensionDiagnosticFact {
        source_kind: ExtensionLoadSourceKind::ProjectMcp,
        source_key: server_name.to_owned(),
        phase,
        severity: ExtensionLoadSeverity::Warning,
        remedy_code,
        // Facts never include raw URL/header/command/args/env/stderr or rmcp errors.
        summary_material: summary_material.to_owned(),
    }
}

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
    connect_and_discover_with_diagnostics(task_short_id, role_name, servers, app_state)
        .await
        .registry
}

/// Diagnostics-capable MCP startup entry point for lifecycle integration.
///
/// It preserves the legacy non-fatal policy: a failed server is skipped and
/// later servers still load. Facts cover only configured-server startup; tool
/// calls, schema serialization, disconnects, and refreshes do not enter here.
pub(crate) async fn connect_and_discover_with_diagnostics(
    task_short_id: &str,
    role_name: &str,
    servers: &[(String, McpServerConfig)],
    app_state: &AgentContext,
) -> McpDiscoveryResult {
    if servers.is_empty() {
        return McpDiscoveryResult {
            registry: None,
            diagnostics: Vec::new(),
        };
    }

    // Create the shared routing state early so notification handlers
    // (spawned during connect_to_server) can hold a reference to it.
    let routing = Arc::new(RwLock::new(RoutingState {
        tool_to_server: HashMap::new(),
        namespaced_to_original: HashMap::new(),
        peers: HashMap::new(),
        request_timeouts: HashMap::new(),
        server_instructions: BTreeMap::new(),
        unavailable: HashSet::new(),
        tool_fingerprints: HashMap::new(),
        resource_servers: HashSet::new(),
    }));

    let mut tool_to_server: HashMap<String, String> = HashMap::new();
    let mut namespaced_to_original: HashMap<String, String> = HashMap::new();
    let mut tool_fingerprints: HashMap<String, u64> = HashMap::new();
    let mut peers: HashMap<String, Arc<Peer<RoleClient>>> = HashMap::new();
    let mut request_timeouts: HashMap<String, u64> = HashMap::new();
    let mut tool_schemas: Vec<serde_json::Value> = Vec::new();
    let mut server_instructions: BTreeMap<String, String> = BTreeMap::new();
    let mut resource_servers_set: HashSet<String> = HashSet::new();
    let mut diagnostics = Vec::new();

    for (name, config) in servers {
        let resolved = match resolve_server_config(name, config, app_state).await {
            Ok(resolved) => resolved,
            Err(missing) => {
                diagnostics.push(mcp_diagnostic(
                    name,
                    ExtensionLoadPhase::PlaceholderResolution,
                    ExtensionLoadRemedyCode::CheckPlaceholder,
                    "A configured MCP placeholder value is unavailable.",
                ));
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
                // No stdio process is launched by this loader, so process_start is not reached.
                diagnostics.push(mcp_diagnostic(
                    name,
                    ExtensionLoadPhase::Transport,
                    ExtensionLoadRemedyCode::CheckTransport,
                    "MCP stdio transport is not supported for agent sessions.",
                ));
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
                diagnostics.push(mcp_diagnostic(
                    name,
                    ExtensionLoadPhase::Transport,
                    ExtensionLoadRemedyCode::CheckTransport,
                    "MCP transport configuration is unsupported.",
                ));
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
        let startup_result = tokio::time::timeout(
            startup_duration,
            startup_and_list(
                &url,
                &resolved.headers,
                name,
                task_short_id,
                routing.clone(),
            ),
        )
        .await;

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
            Ok(Err(failure)) => {
                diagnostics.push(failure.diagnostic(name));
                tracing::warn!(
                    task_id = %task_short_id,
                    role = %role_name,
                    server = %name,
                    url = %url,
                    error = %failure,
                    "Failed to connect to MCP server; skipping"
                );
                continue;
            }
            Err(_elapsed) => {
                // The combined timeout cannot prove initial tools/list was reached.
                diagnostics.push(mcp_diagnostic(
                    name,
                    ExtensionLoadPhase::Handshake,
                    ExtensionLoadRemedyCode::CheckServer,
                    "MCP connection or initialization timed out.",
                ));
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
            // Track servers that advertise the resources capability.
            if info.capabilities.resources.is_some() {
                resource_servers_set.insert(name.clone());
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
                namespaced_to_original.insert(namespaced.clone(), original_name);
                tool_fingerprints.insert(namespaced, compute_tool_fingerprint(&tool));
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
        return McpDiscoveryResult {
            registry: None,
            diagnostics,
        };
    }

    // Populate the shared routing state with all discovered data.
    {
        let mut r = routing.write().unwrap();
        r.tool_to_server = tool_to_server;
        r.namespaced_to_original = namespaced_to_original;
        r.peers = peers;
        r.request_timeouts = request_timeouts;
        r.tool_fingerprints = tool_fingerprints;
        r.server_instructions = server_instructions.clone();
        r.resource_servers = resource_servers_set.clone();
    }

    // Sort resource server names for deterministic order.
    let mut resource_server_names: Vec<String> = resource_servers_set.into_iter().collect();
    resource_server_names.sort();

    McpDiscoveryResult {
        registry: Some(McpToolRegistry {
            routing,
            tool_schemas,
            server_instructions,
            resource_servers: resource_server_names,
            #[cfg(test)]
            test_dispatch: None,
        }),
        diagnostics,
    }
}

/// Establish a connection to an MCP server via Streamable HTTP transport.
///
/// The returned peer uses a [`McpNotificationHandler`] that observes
/// `LoggingMessageNotification`s from the server and emits them through
/// host `tracing` with structured `{server, logger, level, task_short_id}` fields.
///
/// The handler also holds a reference to the shared `routing` state so it can
/// process `tools/list_changed` notifications by issuing a refresh.
async fn connect_to_server(
    url: &str,
    headers: &HashMap<String, String>,
    server_name: &str,
    task_short_id: &str,
    routing: Arc<RwLock<RoutingState>>,
) -> Result<Peer<RoleClient>, McpStartupFailure> {
    let mut custom_headers = HashMap::new();
    for (name, value) in headers {
        let header_name =
            HeaderName::try_from(name.as_str()).map_err(|_| McpStartupFailure::Transport)?;
        let header_value =
            HeaderValue::try_from(value.as_str()).map_err(|_| McpStartupFailure::Transport)?;
        custom_headers.insert(header_name, header_value);
    }

    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
        .custom_headers(custom_headers);
    let transport = StreamableHttpClientTransport::from_config(config);

    let handler = McpNotificationHandler {
        server_name: server_name.to_string(),
        task_short_id: task_short_id.to_string(),
        routing,
    };
    let service = handler
        .serve(transport)
        .await
        .map_err(|_| McpStartupFailure::Handshake)?;
    let peer = service.peer().clone();
    // Keep the service alive in the background so notification processing
    // continues for the lifetime of the connection.
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
    server_name: &str,
    task_short_id: &str,
    routing: Arc<RwLock<RoutingState>>,
) -> Result<(Peer<RoleClient>, rmcp::model::ListToolsResult), McpStartupFailure> {
    let peer = connect_to_server(url, headers, server_name, task_short_id, routing).await?;
    let result = peer
        .list_tools(None)
        .await
        .map_err(|_| McpStartupFailure::ToolsList)?;
    Ok((peer, result))
}

#[cfg(test)]
mod capability_tests;
#[cfg(test)]
mod resource_tests;
#[cfg(test)]
mod tests;
