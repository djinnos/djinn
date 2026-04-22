use crate::server::AppState;
use crate::test_helpers;
use djinn_control_plane::server::DjinnMcpServer;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn test_mcp() -> DjinnMcpServer {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    DjinnMcpServer::new(state.mcp_state())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_task_family() {
    let mcp = test_mcp();
    let result = mcp.dispatch_tool("task_list", json!({"project": "/tmp/nonexistent", "issue_type": "task", "status": "open", "label": "", "text": "", "sort": "updated_at", "offset": 0, "limit": 10})).await;
    assert!(
        result.is_ok(),
        "dispatch_tool task_list returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_epic_family() {
    let mcp = test_mcp();
    let result = mcp
        .dispatch_tool(
            "epic_list",
            json!({"project": "/tmp/nonexistent", "limit": 1}),
        )
        .await;
    assert!(
        result.is_ok(),
        "dispatch_tool epic_list returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_memory_family() {
    let mcp = test_mcp();
    let result = mcp
        .dispatch_tool(
            "memory_search",
            json!({"project":"/tmp/nonexistent", "query":"x", "limit": 1}),
        )
        .await;
    assert!(
        result.is_ok(),
        "dispatch_tool memory_search returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_settings_family() {
    let mcp = test_mcp();
    let result = mcp.dispatch_tool("settings_get", json!({})).await;
    assert!(
        result.is_ok(),
        "dispatch_tool settings_get returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_provider_family() {
    let mcp = test_mcp();
    let result = mcp.dispatch_tool("provider_catalog", json!({})).await;
    assert!(
        result.is_ok(),
        "dispatch_tool provider_catalog returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_rejects_unknown_tool() {
    let mcp = test_mcp();
    let err = mcp
        .dispatch_tool("tool_that_does_not_exist", json!({}))
        .await
        .expect_err("unknown tool should fail");
    assert!(err.contains("unknown MCP tool"));
}
