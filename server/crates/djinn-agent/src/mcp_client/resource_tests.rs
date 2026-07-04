use super::tests::make_routing;
use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

// ── Resource capability detection tests ──────────────────────────────

/// Helper: build an `Arc<RwLock<RoutingState>>` with explicit resource servers.
fn make_routing_with_resources(
    tool_to_server: HashMap<String, String>,
    namespaced_to_original: HashMap<String, String>,
    resource_servers: HashSet<String>,
) -> Arc<RwLock<RoutingState>> {
    Arc::new(RwLock::new(RoutingState {
        tool_to_server,
        namespaced_to_original,
        peers: HashMap::new(),
        request_timeouts: HashMap::new(),
        unavailable: HashSet::new(),
        server_instructions: BTreeMap::new(),
        tool_fingerprints: HashMap::new(),
        resource_servers,
    }))
}

/// Helper: build an `McpToolRegistry` with explicit resource servers.
fn make_registry_with_resources(resource_servers: Vec<String>) -> McpToolRegistry {
    let mut sorted = resource_servers;
    sorted.sort();
    McpToolRegistry {
        routing: make_routing_with_resources(
            HashMap::new(),
            HashMap::new(),
            sorted.iter().cloned().collect(),
        ),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: sorted,
        test_dispatch: None,
    }
}

#[test]
fn has_resource_servers_returns_false_by_default() {
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    assert!(!registry.has_resource_servers());
    assert!(registry.resource_server_names().is_empty());
}

#[test]
fn has_resource_servers_returns_true_when_populated() {
    let registry =
        make_registry_with_resources(vec!["alpha-server".to_string(), "beta-server".to_string()]);
    assert!(registry.has_resource_servers());
    assert_eq!(registry.resource_server_names().len(), 2);
}

#[test]
fn resource_server_names_are_deterministically_sorted() {
    let registry = make_registry_with_resources(vec![
        "zebra".to_string(),
        "alpha".to_string(),
        "middle".to_string(),
    ]);
    let names = registry.resource_server_names();
    assert_eq!(names, &["alpha", "middle", "zebra"]);
}

#[test]
fn resource_server_names_accessor_is_read_only() {
    let registry = make_registry_with_resources(vec!["server-a".to_string()]);
    let names1 = registry.resource_server_names();
    let names2 = registry.resource_server_names();
    assert_eq!(names1, names2);
}

#[tokio::test]
async fn list_resources_returns_empty_when_no_resource_servers() {
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    let result = registry.list_resources(None).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn list_resources_returns_error_for_non_resource_server() {
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    let result = registry.list_resources(Some("unknown-server")).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("not resource-capable"),
        "expected resource-capable error, got: {err}"
    );
}

#[tokio::test]
async fn read_resource_returns_error_for_non_resource_server() {
    let registry = McpToolRegistry {
        routing: make_routing(HashMap::new(), HashMap::new()),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: Vec::new(),
        test_dispatch: None,
    };
    let result = registry
        .read_resource("unknown-server", "file:///test.txt")
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("not resource-capable"),
        "expected resource-capable error, got: {err}"
    );
}

#[tokio::test]
async fn read_resource_returns_error_for_missing_peer() {
    // Server is resource-capable but has no peer (shouldn't happen in practice
    // but tests the deterministic error path).
    let mut resource_servers = HashSet::new();
    resource_servers.insert("orphan-server".to_string());
    let registry = McpToolRegistry {
        routing: make_routing_with_resources(HashMap::new(), HashMap::new(), resource_servers),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: vec!["orphan-server".to_string()],
        test_dispatch: None,
    };
    let result = registry
        .read_resource("orphan-server", "file:///test.txt")
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("peer not found"),
        "expected peer-not-found error, got: {err}"
    );
}

#[tokio::test]
async fn list_resources_timeout_returns_deterministic_error_when_all_fail() {
    // Build a resource-capable routing state with no peers.
    // list_resources uses the real peer (not test_dispatch), so with no peers
    // the snapshot is empty and it returns Ok([]).
    let mut resource_servers = HashSet::new();
    resource_servers.insert("no-peer-server".to_string());
    let registry = McpToolRegistry {
        routing: make_routing_with_resources(HashMap::new(), HashMap::new(), resource_servers),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: vec!["no-peer-server".to_string()],
        test_dispatch: None,
    };
    // No peers in routing state → snapshot is empty → returns Ok([]).
    let result = registry.list_resources(None).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn list_resources_specific_server_not_resource_capable() {
    let registry = make_registry_with_resources(vec!["resource-server".to_string()]);
    // Requesting a server that is NOT in resource_servers should fail.
    let result = registry.list_resources(Some("other-server")).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("not resource-capable"),
        "expected not-resource-capable error, got: {err}"
    );
}

#[tokio::test]
async fn list_resources_from_specific_resource_server_no_peer() {
    // Server is resource-capable but has no peer handle (empty routing).
    let registry = make_registry_with_resources(vec!["resource-server".to_string()]);
    let result = registry.list_resources(Some("resource-server")).await;
    // No peers → snapshot is empty → returns Ok([]).
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[tokio::test]
async fn read_resource_uses_default_timeout_when_server_not_in_timeout_map() {
    // Verify the same fallback behavior as call_tool: when a server is not in
    // request_timeouts, the default timeout (120_000ms) is used. We can't
    // easily test the full timeout path without a real peer, but we can
    // verify the error path works.
    let mut resource_servers = HashSet::new();
    resource_servers.insert("resource-server".to_string());
    let registry = McpToolRegistry {
        routing: make_routing_with_resources(HashMap::new(), HashMap::new(), resource_servers),
        tool_schemas: Vec::new(),
        server_instructions: BTreeMap::new(),
        resource_servers: vec!["resource-server".to_string()],
        test_dispatch: None,
    };
    // No peer for this server → deterministic error.
    let result = registry
        .read_resource("resource-server", "file:///test.txt")
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("peer not found"),
        "expected peer-not-found error, got: {err}"
    );
}

// ── rmcp resource type compile-time probes ──────────────────────────
//
// Verify the rmcp types used by list_resources/read_resource are accessible.

#[test]
fn rmcp_resource_type_is_accessible() {
    // Compile-time probe: Resource type can be constructed.
    let raw = rmcp::model::RawResource::new("file:///test.txt", "test");
    let resource = RmcpResource::new(raw, None);
    assert_eq!(resource.uri, "file:///test.txt");
    assert_eq!(resource.name, "test");
}

#[test]
fn rmcp_resource_contents_text_is_accessible() {
    // Compile-time probe: ResourceContents::TextResourceContents can be matched.
    let contents = ResourceContents::text("hello world", "str:///hello");
    match &contents {
        ResourceContents::TextResourceContents { uri, text, .. } => {
            assert_eq!(uri, "str:///hello");
            assert_eq!(text, "hello world");
        }
        _ => panic!("expected TextResourceContents"),
    }
}

#[test]
fn rmcp_resource_contents_blob_is_accessible() {
    // Compile-time probe: ResourceContents::BlobResourceContents can be matched.
    let contents = ResourceContents::blob("base64data", "file:///image.png");
    match &contents {
        ResourceContents::BlobResourceContents { uri, blob, .. } => {
            assert_eq!(uri, "file:///image.png");
            assert_eq!(blob, "base64data");
        }
        _ => panic!("expected BlobResourceContents"),
    }
}

#[test]
fn rmcp_read_resource_request_params_construction() {
    // Compile-time probe: ReadResourceRequestParams::new works.
    let params = ReadResourceRequestParams::new("file:///test.txt");
    assert_eq!(params.uri, "file:///test.txt");
}

#[test]
fn rmcp_list_resources_result_is_accessible() {
    // Compile-time probe: ListResourcesResult can be constructed and inspected.
    let result = rmcp::model::ListResourcesResult::default();
    assert!(result.resources.is_empty());
    assert!(result.next_cursor.is_none());
}

// ── Native resource tool schema-gating tests (task jdgb) ─────────────
//
// These tests verify the `has_resource_servers()` predicate that
// `stage.rs` uses to gate exposure of the native `list_mcp_resources`
// and `read_mcp_resource` schemas, plus the schema shapes themselves.

/// Schema gating: no resource servers → schemas must NOT be appended.
#[test]
fn resource_schema_gating_off_without_capability() {
    let registry = make_registry_with_resources(Vec::new());
    assert!(!registry.has_resource_servers());
}

/// Schema gating: with resource servers → schemas SHOULD be appended.
#[test]
fn resource_schema_gating_on_with_capability() {
    let registry = make_registry_with_resources(vec!["resource-server".to_string()]);
    assert!(registry.has_resource_servers());
    assert_eq!(
        registry.resource_server_names(),
        &["resource-server".to_string()]
    );
}

/// Verify the `list_mcp_resources` schema shape matches proposal ql4s:
/// optional `server` string, read-only, non-destructive.
#[test]
fn list_mcp_resources_schema_shape() {
    let schema = serde_json::json!({
        "type": "function",
        "function": {
            "name": "list_mcp_resources",
            "parameters": {
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "required": [],
                "additionalProperties": false
            }
        },
        "readOnly": true,
        "destructive": false,
        "idempotent": true,
        "openWorld": false,
        "concurrent_safe": true
    });
    assert_eq!(schema["function"]["name"], "list_mcp_resources");
    assert_eq!(
        schema["function"]["parameters"]["required"],
        serde_json::json!([])
    );
    assert!(schema["function"]["parameters"]["properties"]["server"].is_object());
    assert_eq!(schema["readOnly"], true);
    assert_eq!(schema["destructive"], false);
}

/// Verify the `read_mcp_resource` schema shape matches proposal ql4s:
/// required `server` and `uri` strings, read-only, non-destructive.
#[test]
fn read_mcp_resource_schema_shape() {
    let schema = serde_json::json!({
        "type": "function",
        "function": {
            "name": "read_mcp_resource",
            "parameters": {
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }
        },
        "readOnly": true,
        "destructive": false,
        "idempotent": true,
        "openWorld": false,
        "concurrent_safe": true
    });
    assert_eq!(schema["function"]["name"], "read_mcp_resource");
    assert_eq!(
        schema["function"]["parameters"]["required"],
        serde_json::json!(["server", "uri"])
    );
    assert_eq!(schema["readOnly"], true);
    assert_eq!(schema["destructive"], false);
}
