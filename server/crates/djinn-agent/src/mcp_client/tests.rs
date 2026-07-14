use super::config::ResolvedMcpServerConfig;
use super::*;
use crate::test_helpers::{agent_context_from_db, create_test_db};
use axum::Router;
use djinn_core::events::EventBus;
use djinn_provider::repos::CredentialRepository;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    object, schemars, tool, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn test_context() -> AgentContext {
    agent_context_from_db(create_test_db(), CancellationToken::new())
}

#[derive(Clone)]
struct StartupFixture {
    tool_router: ToolRouter<Self>,
}
impl StartupFixture {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct FixtureArguments {}
#[tool_router]
impl StartupFixture {
    #[tool(description = "fixture tool")]
    fn fixture_tool(&self, Parameters(_): Parameters<FixtureArguments>) -> String {
        "ok".to_owned()
    }
}
impl ServerHandler for StartupFixture {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}
async fn spawn_startup_fixture() -> (String, CancellationToken) {
    let cancellation = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_stateful_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_cancellation_token(cancellation.child_token());
    let service: StreamableHttpService<StartupFixture, LocalSessionManager> =
        StreamableHttpService::new(|| Ok(StartupFixture::new()), Default::default(), config);
    let router = Router::new().nest_service("/mcp", service);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let address = listener.local_addr().expect("fixture address");
    let shutdown = cancellation.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    (format!("http://{address}/mcp"), cancellation)
}

/// Accept HTTP connections without replying, so the loader's startup timeout
/// covers a real in-flight transport/initialize attempt.
async fn spawn_unresponsive_http_fixture() -> (String, CancellationToken) {
    let cancellation = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unresponsive fixture");
    let address = listener.local_addr().expect("unresponsive fixture address");
    let shutdown = cancellation.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let connection_shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            let _stream = stream;
                            connection_shutdown.cancelled().await;
                        });
                    }
                    Err(_) => break,
                },
            }
        }
    });
    (format!("http://{address}/mcp"), cancellation)
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
pub(super) fn make_routing(
    tool_to_server: HashMap<String, String>,
    namespaced_to_original: HashMap<String, String>,
) -> Arc<RwLock<RoutingState>> {
    Arc::new(RwLock::new(RoutingState {
        tool_to_server,
        namespaced_to_original,
        peers: HashMap::new(),
        request_timeouts: HashMap::new(),
        unavailable: HashSet::new(),
        server_instructions: BTreeMap::new(),
        tool_fingerprints: HashMap::new(),
        resource_servers: HashSet::new(),
    }))
}

#[test]
fn empty_registry_has_no_tools() {
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
            server_instructions: BTreeMap::new(),
            resource_servers: Vec::new(),
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
        "test-server",
        "test-task",
        make_routing(HashMap::new(), HashMap::new()),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
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

/// Helper: build an `Arc<RwLock<RoutingState>>` with explicit fingerprints.
fn make_routing_with_fingerprints(
    tool_to_server: HashMap<String, String>,
    namespaced_to_original: HashMap<String, String>,
    tool_fingerprints: HashMap<String, u64>,
) -> Arc<RwLock<RoutingState>> {
    Arc::new(RwLock::new(RoutingState {
        tool_to_server,
        namespaced_to_original,
        peers: HashMap::new(),
        request_timeouts: HashMap::new(),
        unavailable: HashSet::new(),
        server_instructions: BTreeMap::new(),
        tool_fingerprints,
        resource_servers: HashSet::new(),
    }))
}

// ── tools/list_changed rename-routing tests ──────────────────────────

#[test]
fn apply_tools_list_result_unchanged_tools_remain() {
    // Setup: one tool registered for "my-server".
    let ns_stable = mcp_namespaced_name("my-server", "stable_tool");
    let tool_to_server = HashMap::from([(ns_stable.clone(), "my-server".to_string())]);
    let namespaced_to_original = HashMap::from([(ns_stable.clone(), "stable_tool".to_string())]);
    let fp = 12345u64;
    let tool_fingerprints = HashMap::from([(ns_stable.clone(), fp)]);

    let routing =
        make_routing_with_fingerprints(tool_to_server, namespaced_to_original, tool_fingerprints);

    // Refresh: server still advertises stable_tool with the same fingerprint.
    let new_tool = RmcpTool::new(
        "stable_tool".to_string(),
        "does something".to_string(),
        object!({"type": "object"}),
    );
    let (unavailable, renamed) = routing
        .write()
        .unwrap()
        .apply_tools_list_result("my-server", &[new_tool]);

    assert!(unavailable.is_empty(), "no tools should be removed");
    assert!(renamed.is_empty(), "no tools should be renamed");
    assert!(!routing.read().unwrap().unavailable.contains(&ns_stable));
}

#[test]
fn apply_tools_list_result_removed_tool_marked_unavailable() {
    let ns_gone = mcp_namespaced_name("my-server", "gone_tool");
    let tool_to_server = HashMap::from([(ns_gone.clone(), "my-server".to_string())]);
    let namespaced_to_original = HashMap::from([(ns_gone.clone(), "gone_tool".to_string())]);
    let fp = 99999u64;
    let tool_fingerprints = HashMap::from([(ns_gone.clone(), fp)]);

    let routing =
        make_routing_with_fingerprints(tool_to_server, namespaced_to_original, tool_fingerprints);

    // Refresh: server no longer advertises gone_tool, and no new tool shares
    // its fingerprint.
    let new_tool = RmcpTool::new(
        "completely_different".to_string(),
        "something unrelated".to_string(),
        object!({"type": "object"}),
    );
    let (unavailable, renamed) = routing
        .write()
        .unwrap()
        .apply_tools_list_result("my-server", &[new_tool]);

    assert_eq!(unavailable, vec![ns_gone.clone()]);
    assert!(renamed.is_empty());
    assert!(routing.read().unwrap().unavailable.contains(&ns_gone));
}

#[test]
fn apply_tools_list_result_new_tools_not_registered() {
    // Setup: one existing tool.
    let ns_existing = mcp_namespaced_name("my-server", "existing");
    let tool_to_server = HashMap::from([(ns_existing.clone(), "my-server".to_string())]);
    let namespaced_to_original = HashMap::from([(ns_existing.clone(), "existing".to_string())]);
    let tool_fingerprints = HashMap::from([(ns_existing.clone(), 111u64)]);

    let routing =
        make_routing_with_fingerprints(tool_to_server, namespaced_to_original, tool_fingerprints);

    // Refresh: server advertises existing + a brand new tool.
    let existing = RmcpTool::new(
        "existing".to_string(),
        "still here".to_string(),
        object!({"type": "object"}),
    );
    let new_tool = RmcpTool::new(
        "brand_new".to_string(),
        "freshly added".to_string(),
        object!({"type": "string"}),
    );
    let (unavailable, renamed) = routing
        .write()
        .unwrap()
        .apply_tools_list_result("my-server", &[existing, new_tool]);

    assert!(unavailable.is_empty());
    assert!(renamed.is_empty());
    // brand_new should NOT be in the routing maps.
    let ns_new = mcp_namespaced_name("my-server", "brand_new");
    assert!(
        !routing.read().unwrap().tool_to_server.contains_key(&ns_new),
        "newly discovered tools must not be registered"
    );
}

#[test]
fn apply_tools_list_result_provably_renamed_tool_routes_through_old_alias() {
    // Setup: "old_name" tool with a known fingerprint.
    let ns_old = mcp_namespaced_name("my-server", "old_name");
    let tool_to_server = HashMap::from([(ns_old.clone(), "my-server".to_string())]);
    let namespaced_to_original = HashMap::from([(ns_old.clone(), "old_name".to_string())]);

    // Compute the fingerprint from the original tool definition.
    let original_tool = RmcpTool::new(
        "old_name".to_string(),
        "does the thing".to_string(),
        object!({"type": "object", "properties": {"x": {"type": "number"}}}),
    );
    let fp = compute_tool_fingerprint(&original_tool);
    let tool_fingerprints = HashMap::from([(ns_old.clone(), fp)]);

    let routing =
        make_routing_with_fingerprints(tool_to_server, namespaced_to_original, tool_fingerprints);

    // Refresh: server now advertises "new_name" with the same description and
    // input_schema (same fingerprint).  old_name is gone.
    let renamed_tool = RmcpTool::new(
        "new_name".to_string(),
        "does the thing".to_string(),
        object!({"type": "object", "properties": {"x": {"type": "number"}}}),
    );
    let (unavailable, renamed) = routing
        .write()
        .unwrap()
        .apply_tools_list_result("my-server", &[renamed_tool]);

    assert!(
        unavailable.is_empty(),
        "renamed tool should not be unavailable"
    );
    assert_eq!(renamed, vec![ns_old.clone()], "should detect the rename");

    // The old namespaced alias now routes to the new original name.
    let r = routing.read().unwrap();
    assert_eq!(
        r.namespaced_to_original.get(&ns_old).map(String::as_str),
        Some("new_name"),
        "old alias should route to the new tool name"
    );
    assert!(!r.unavailable.contains(&ns_old));
}

#[test]
fn apply_tools_list_result_ambiguous_rename_marks_unavailable() {
    // Setup: "original_tool" with a fingerprint.
    let ns_orig = mcp_namespaced_name("my-server", "original_tool");
    let tool_to_server = HashMap::from([(ns_orig.clone(), "my-server".to_string())]);
    let namespaced_to_original = HashMap::from([(ns_orig.clone(), "original_tool".to_string())]);

    let original_tool = RmcpTool::new(
        "original_tool".to_string(),
        "generic helper".to_string(),
        object!({"type": "object"}),
    );
    let fp = compute_tool_fingerprint(&original_tool);
    let tool_fingerprints = HashMap::from([(ns_orig.clone(), fp)]);

    let routing =
        make_routing_with_fingerprints(tool_to_server, namespaced_to_original, tool_fingerprints);

    // Refresh: two new tools share the same fingerprint (ambiguous).
    let candidate_a = RmcpTool::new(
        "helper_v2".to_string(),
        "generic helper".to_string(),
        object!({"type": "object"}),
    );
    let candidate_b = RmcpTool::new(
        "helper_v3".to_string(),
        "generic helper".to_string(),
        object!({"type": "object"}),
    );
    let (unavailable, renamed) = routing
        .write()
        .unwrap()
        .apply_tools_list_result("my-server", &[candidate_a, candidate_b]);

    assert_eq!(
        unavailable,
        vec![ns_orig.clone()],
        "ambiguous rename should mark as unavailable"
    );
    assert!(renamed.is_empty(), "ambiguous match should not rename");
    assert!(routing.read().unwrap().unavailable.contains(&ns_orig));
}

#[test]
fn apply_tools_list_result_no_fingerprint_marks_removed() {
    // If a tool was registered without a fingerprint (edge case), it is
    // treated as removed when its name disappears.
    let ns_legacy = mcp_namespaced_name("my-server", "legacy_tool");
    let tool_to_server = HashMap::from([(ns_legacy.clone(), "my-server".to_string())]);
    let namespaced_to_original = HashMap::from([(ns_legacy.clone(), "legacy_tool".to_string())]);
    // Empty fingerprints map — no fingerprint stored.
    let tool_fingerprints = HashMap::new();

    let routing =
        make_routing_with_fingerprints(tool_to_server, namespaced_to_original, tool_fingerprints);

    let (unavailable, renamed) = routing
        .write()
        .unwrap()
        .apply_tools_list_result("my-server", &[]);

    assert_eq!(unavailable, vec![ns_legacy.clone()]);
    assert!(renamed.is_empty());
}

#[test]
fn apply_tools_list_result_session_fixed_schemas_unchanged() {
    // tool_schemas is on McpToolRegistry, not RoutingState.  Verify that
    // the McpToolRegistry-level apply_tools_refresh (name-only) does not
    // modify tool_schemas, and that apply_tools_list_result on RoutingState
    // has no way to modify them either.
    let ns_tool = mcp_namespaced_name("my-server", "my_tool");
    let tool_to_server = HashMap::from([(ns_tool.clone(), "my-server".to_string())]);
    let namespaced_to_original = HashMap::from([(ns_tool.clone(), "my_tool".to_string())]);

    let registry = McpToolRegistry {
        routing: make_routing(tool_to_server, namespaced_to_original),
        tool_schemas: vec![serde_json::json!({"name": ns_tool})],
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: None,
    };

    let schemas_before = registry.tool_schemas().len();

    // apply_tools_refresh (name-only path)
    registry.apply_tools_refresh("my-server", &["brand_new".to_string()]);
    assert_eq!(registry.tool_schemas().len(), schemas_before);

    // apply_tools_list_result (full path) on the shared routing state
    {
        let new_tool = RmcpTool::new(
            "brand_new".to_string(),
            "freshly added".to_string(),
            object!({"type": "string"}),
        );
        registry
            .routing
            .write()
            .unwrap()
            .apply_tools_list_result("my-server", &[new_tool]);
    }
    assert_eq!(
        registry.tool_schemas().len(),
        schemas_before,
        "tool_schemas must remain session-fixed after any refresh path"
    );
}

// ── Timeout enforcement tests ──────────────────────────────────────

/// Helper: build an `Arc<RwLock<RoutingState>>` from raw maps with
/// explicit per-server request timeouts.
fn make_routing_with_timeouts(
    tool_to_server: HashMap<String, String>,
    namespaced_to_original: HashMap<String, String>,
    request_timeouts: HashMap<String, u64>,
) -> Arc<RwLock<RoutingState>> {
    Arc::new(RwLock::new(RoutingState {
        tool_to_server,
        namespaced_to_original,
        peers: HashMap::new(),
        request_timeouts,
        unavailable: HashSet::new(),
        server_instructions: BTreeMap::new(),
        tool_fingerprints: HashMap::new(),
        resource_servers: HashSet::new(),
    }))
}

#[tokio::test]
async fn call_tool_timeout_returns_deterministic_error() {
    let namespaced = mcp_namespaced_name("slow-server", "slow_tool");
    let mut tool_to_server = HashMap::new();
    tool_to_server.insert(namespaced.clone(), "slow-server".to_string());
    let namespaced_to_original = HashMap::from([(namespaced.clone(), "slow_tool".to_string())]);
    // 50ms timeout — the test dispatch will hang longer.
    let request_timeouts = HashMap::from([("slow-server".to_string(), 50u64)]);

    let registry = McpToolRegistry {
        routing: make_routing_with_timeouts(
            tool_to_server,
            namespaced_to_original,
            request_timeouts,
        ),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: Some(Arc::new(move |_, _| {
            Box::pin(async {
                // Simulate a slow server that takes longer than the timeout.
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(serde_json::json!({"ok": true}))
            })
        })),
    };

    let result = registry.call_tool(&namespaced, None).await;
    assert!(result.is_err(), "call_tool should return Err on timeout");
    let err = result.unwrap_err();
    assert!(
        err.contains("timed out"),
        "error should mention timeout, got: {err}"
    );
    assert!(
        err.contains(&namespaced),
        "error should include tool name, got: {err}"
    );
    assert!(
        err.contains("slow-server"),
        "error should include server name, got: {err}"
    );
}

#[tokio::test]
async fn call_tool_uses_default_timeout_when_server_not_in_map() {
    let namespaced = mcp_namespaced_name("default-server", "fast_tool");
    let mut tool_to_server = HashMap::new();
    tool_to_server.insert(namespaced.clone(), "default-server".to_string());
    let namespaced_to_original = HashMap::from([(namespaced.clone(), "fast_tool".to_string())]);
    // Empty request_timeouts map — should fall back to default (120_000ms).
    let request_timeouts = HashMap::new();

    let registry = McpToolRegistry {
        routing: make_routing_with_timeouts(
            tool_to_server,
            namespaced_to_original,
            request_timeouts,
        ),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: Some(Arc::new(|_, _| {
            Box::pin(async { Ok(serde_json::json!({"fast": true})) })
        })),
    };

    // Should succeed quickly — the default timeout (120s) is not reached.
    let result = registry.call_tool(&namespaced, None).await;
    assert!(result.is_ok(), "fast dispatch should not timeout");
    assert_eq!(result.unwrap(), serde_json::json!({"fast": true}));
}

#[tokio::test]
async fn request_timeout_stored_per_server_at_discovery() {
    let mut routing_state = RoutingState {
        tool_to_server: HashMap::new(),
        namespaced_to_original: HashMap::new(),
        peers: HashMap::new(),
        request_timeouts: HashMap::new(),
        unavailable: HashSet::new(),
        server_instructions: BTreeMap::new(),
        tool_fingerprints: HashMap::new(),
        resource_servers: HashSet::new(),
    };
    routing_state
        .request_timeouts
        .insert("server-a".to_string(), 5_000);
    routing_state
        .request_timeouts
        .insert("server-b".to_string(), 60_000);

    assert_eq!(routing_state.request_timeouts.get("server-a"), Some(&5_000));
    assert_eq!(
        routing_state.request_timeouts.get("server-b"),
        Some(&60_000)
    );
    // Unknown server should not be present.
    assert!(!routing_state.request_timeouts.contains_key("server-c"));
}

// ── Server instructions accessor tests ─────────────────────────────

#[test]
fn server_instructions_accessor_returns_empty_by_default() {
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    assert!(
        registry.server_instructions().is_empty(),
        "registry with no instructions should return empty map"
    );
}

#[test]
fn server_instructions_accessor_returns_populated_map() {
    let mut instructions = BTreeMap::new();
    instructions.insert(
        "search-server".to_string(),
        "Use web_search for live information.".to_string(),
    );
    instructions.insert(
        "code-server".to_string(),
        "Use code_search for repository lookup.".to_string(),
    );
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: instructions,
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    let result = registry.server_instructions();
    assert_eq!(result.len(), 2);
    assert_eq!(
        result.get("search-server").map(String::as_str),
        Some("Use web_search for live information.")
    );
    assert_eq!(
        result.get("code-server").map(String::as_str),
        Some("Use code_search for repository lookup.")
    );
}

#[test]
fn server_instructions_accessor_returns_btree_sorted_keys() {
    // Insert in reverse-alphabetical order; BTreeMap sorts by key.
    let mut instructions = BTreeMap::new();
    instructions.insert("zebra".to_string(), "Zebra instr.".to_string());
    instructions.insert("alpha".to_string(), "Alpha instr.".to_string());
    instructions.insert("middle".to_string(), "Middle instr.".to_string());
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: instructions,
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    let keys: Vec<&str> = registry
        .server_instructions()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["alpha", "middle", "zebra"]);
}

#[test]
fn server_instructions_clone_shares_same_data() {
    let mut instructions = BTreeMap::new();
    instructions.insert(
        "shared-server".to_string(),
        "Shared instructions.".to_string(),
    );
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: instructions,
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    let clone = registry.clone();
    assert_eq!(
        clone
            .server_instructions()
            .get("shared-server")
            .map(String::as_str),
        Some("Shared instructions.")
    );
}

#[test]
fn startup_diagnostic_facts_are_canonical_and_exclude_runtime_paths() {
    let placeholder = mcp_diagnostic(
        "project-server",
        ExtensionLoadPhase::PlaceholderResolution,
        ExtensionLoadRemedyCode::CheckPlaceholder,
        "A configured MCP placeholder value is unavailable.",
    );
    let transport = McpStartupFailure::Transport.diagnostic("project-server");
    let handshake = McpStartupFailure::Handshake.diagnostic("project-server");
    let initial_list = McpStartupFailure::ToolsList.diagnostic("project-server");

    for fact in [&placeholder, &transport, &handshake, &initial_list] {
        assert_eq!(fact.source_kind, ExtensionLoadSourceKind::ProjectMcp);
        assert_eq!(fact.source_key, "project-server");
        assert_eq!(fact.severity, ExtensionLoadSeverity::Warning);
        assert!(!fact.summary_material.contains("Authorization"));
        assert!(!fact.summary_material.contains("Bearer"));
    }
    assert_eq!(placeholder.phase, ExtensionLoadPhase::PlaceholderResolution);
    assert_eq!(
        placeholder.remedy_code,
        ExtensionLoadRemedyCode::CheckPlaceholder
    );
    assert_eq!(transport.phase, ExtensionLoadPhase::Transport);
    assert_eq!(
        transport.remedy_code,
        ExtensionLoadRemedyCode::CheckTransport
    );
    assert_eq!(handshake.phase, ExtensionLoadPhase::Handshake);
    assert_eq!(handshake.remedy_code, ExtensionLoadRemedyCode::CheckServer);
    assert_eq!(initial_list.phase, ExtensionLoadPhase::ToolsList);
    assert_eq!(
        initial_list.remedy_code,
        ExtensionLoadRemedyCode::CheckServer
    );
}

#[tokio::test]
async fn diagnostics_entry_point_keeps_runtime_disconnect_invocation_and_refresh_out_of_facts() {
    let app_state = test_context();
    let (url, shutdown) = spawn_startup_fixture().await;
    let servers = vec![(
        "fixture-server".to_owned(),
        McpServerConfig {
            url: Some(url),
            ..Default::default()
        },
    )];
    let discovery =
        connect_and_discover_with_diagnostics("test", "worker", &servers, &app_state).await;
    assert!(
        discovery.diagnostics.is_empty(),
        "successful startup has no facts"
    );
    let registry = discovery
        .registry
        .expect("initial discovery supplies a registry");
    let tool = mcp_namespaced_name("fixture-server", "fixture_tool");
    assert!(
        registry.has_tool(&tool),
        "initial tools/list discovers the fixture tool"
    );
    shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        registry.call_tool(&tool, None).await.is_err(),
        "post-discovery invocation fails"
    );
    let peer = registry.routing.read().unwrap().peers["fixture-server"].clone();
    assert!(
        refresh_tools_list(&peer, Duration::from_millis(100))
            .await
            .is_err(),
        "post-discovery refresh fails"
    );
    assert!(
        discovery.diagnostics.is_empty(),
        "runtime failures do not create startup observations"
    );
}

#[tokio::test]
async fn diagnostics_entry_point_times_out_one_server_and_discovers_the_next() {
    let app_state = test_context();
    let (slow_url, slow_shutdown) = spawn_unresponsive_http_fixture().await;
    let (good_url, good_shutdown) = spawn_startup_fixture().await;
    let servers = vec![
        (
            "slow-server".to_owned(),
            McpServerConfig {
                url: Some(slow_url),
                startup_timeout_ms: 25,
                ..Default::default()
            },
        ),
        (
            "good-server".to_owned(),
            McpServerConfig {
                url: Some(good_url),
                ..Default::default()
            },
        ),
    ];

    let discovery = tokio::time::timeout(
        Duration::from_secs(1),
        connect_and_discover_with_diagnostics("test", "worker", &servers, &app_state),
    )
    .await
    .expect("short configured startup timeout bounds an unresponsive server");

    assert_eq!(discovery.diagnostics.len(), 1);
    let diagnostic = &discovery.diagnostics[0];
    assert_eq!(diagnostic.source_kind, ExtensionLoadSourceKind::ProjectMcp);
    assert_eq!(diagnostic.source_key, "slow-server");
    assert_eq!(diagnostic.phase, ExtensionLoadPhase::Handshake);
    assert_eq!(diagnostic.severity, ExtensionLoadSeverity::Warning);
    assert_eq!(diagnostic.remedy_code, ExtensionLoadRemedyCode::CheckServer);
    assert_eq!(
        diagnostic.summary_material,
        "MCP connection or initialization timed out."
    );

    let registry = discovery
        .registry
        .expect("a timed-out server does not prevent later discovery");
    assert!(registry.has_tool(&mcp_namespaced_name("good-server", "fixture_tool")));

    slow_shutdown.cancel();
    good_shutdown.cancel();
}
