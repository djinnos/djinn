//! Contract tests for `settings_*` MCP tools.
//!
//! Only the read-side `settings_get` migrated here: the `settings_set` /
//! `settings_reset` tests exercise `RuntimeOps::apply_settings` and
//! `reset_runtime_settings`, which the harness' `StubRuntime` deliberately
//! errors (for set) or no-ops (for reset).  Those stay in
//! `server/src/mcp_contract_tests/settings_tools.rs` because they need the
//! real runtime's provider-credential and mount-config validation paths.

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn settings_get_missing_returns_not_exists() {
    let harness = McpTestHarness::new().await;

    let res = harness
        .call_tool("settings_get", json!({}))
        .await
        .expect("settings_get should dispatch");
    assert_eq!(res["exists"], false);
    assert!(res["settings"].is_null());
}
