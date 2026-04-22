//! Contract tests for `system_*` MCP tools.
//!
//! Migrated from `server/src/mcp_contract_tests/system_tools.rs` — these were
//! pure tool-dispatch assertions and never needed the Axum harness.

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn system_ping_returns_version() {
    let harness = McpTestHarness::new().await;

    let res = harness
        .call_tool("system_ping", json!({}))
        .await
        .expect("system_ping should dispatch cleanly");
    assert_eq!(res["status"], "ok");
    assert_eq!(res["version"], env!("CARGO_PKG_VERSION"));
}
