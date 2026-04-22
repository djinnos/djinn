//! Smoke test for `test_support::McpTestHarness`.
//!
//! Integration tests in a crate's `tests/` directory compile as a separate
//! crate *against* the library; `#[cfg(test)]` on the library does NOT apply
//! here, so the harness MUST be gated with `feature = "test-support"`.  Run
//! this file with:
//!
//! ```text
//! cargo test -p djinn-control-plane --features test-support --test harness_smoke
//! ```

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn system_ping_round_trips_through_harness() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool("system_ping", json!({}))
        .await
        .expect("system_ping should dispatch cleanly through the stub harness");

    // system_ping returns {status: "ok", version: "<CARGO_PKG_VERSION>"}.
    assert_eq!(
        result.get("status").and_then(|v| v.as_str()),
        Some("ok"),
        "expected status=ok, got {result}"
    );
    assert!(
        result
            .get("version")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty()),
        "expected non-empty version, got {result}"
    );
}

#[tokio::test]
async fn unknown_tool_reports_clear_error() {
    let harness = McpTestHarness::new().await;

    let err = harness
        .call_tool("tool_that_does_not_exist", json!({}))
        .await
        .expect_err("unknown tool must fail dispatch");

    let msg = err.to_string();
    assert!(
        msg.contains("tool_that_does_not_exist"),
        "error should name the tool, got: {msg}"
    );
}
