//! MCP tool dispatch smoke tests.
//!
//! Chat is user-scoped under chat-user-global, so the tool-arg `project`
//! passed here is a test project's slug — created via
//! `ProjectRepository::create_with_id`.  The MCP tools don't actually
//! load the project on disk for these lookups (task/epic/memory queries
//! resolve by project id, which is NULL-safe for the empty DB path),
//! but the project_id scaffold exists for future tools that do.

use crate::events::EventBus;
use crate::server::AppState;
use crate::test_helpers;
use djinn_control_plane::server::DjinnMcpServer;
use djinn_db::ProjectRepository;
use serde_json::json;
use tokio_util::sync::CancellationToken;

async fn test_mcp_with_seeded_project() -> (DjinnMcpServer, String) {
    let db = test_helpers::create_test_db();
    let state = AppState::new(db.clone(), CancellationToken::new());
    db.ensure_initialized().await.unwrap();
    let repo = ProjectRepository::new(db, EventBus::noop());
    let project = repo
        .create_with_id("mcp-dispatch", "mcp-dispatch", "test", "mcp-dispatch")
        .await
        .expect("seed project");
    let mcp = DjinnMcpServer::new(state.mcp_state());
    (mcp, project.slug())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_task_family() {
    let (mcp, slug) = test_mcp_with_seeded_project().await;
    let result = mcp.dispatch_tool("task_list", json!({"project": slug, "issue_type": "task", "status": "open", "label": "", "text": "", "sort": "updated_at", "offset": 0, "limit": 10})).await;
    assert!(
        result.is_ok(),
        "dispatch_tool task_list returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_epic_family() {
    let (mcp, slug) = test_mcp_with_seeded_project().await;
    let result = mcp
        .dispatch_tool("epic_list", json!({"project": slug, "limit": 1}))
        .await;
    assert!(
        result.is_ok(),
        "dispatch_tool epic_list returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_memory_family() {
    let (mcp, slug) = test_mcp_with_seeded_project().await;
    let result = mcp
        .dispatch_tool(
            "memory_search",
            json!({"project": slug, "query": "x", "limit": 1}),
        )
        .await;
    assert!(
        result.is_ok(),
        "dispatch_tool memory_search returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_settings_family() {
    let (mcp, _slug) = test_mcp_with_seeded_project().await;
    let result = mcp.dispatch_tool("settings_get", json!({})).await;
    assert!(
        result.is_ok(),
        "dispatch_tool settings_get returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_routes_provider_family() {
    let (mcp, _slug) = test_mcp_with_seeded_project().await;
    let result = mcp.dispatch_tool("provider_catalog", json!({})).await;
    assert!(
        result.is_ok(),
        "dispatch_tool provider_catalog returned error: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_tool_rejects_unknown_tool() {
    let (mcp, _slug) = test_mcp_with_seeded_project().await;
    let err = mcp
        .dispatch_tool("tool_that_does_not_exist", json!({}))
        .await
        .expect_err("unknown tool should fail");
    assert!(err.contains("unknown MCP tool"));
}
