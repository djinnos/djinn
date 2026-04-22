//! Contract tests for `execution_*` MCP tools.
//!
//! `execution_kill_task` with a nonexistent task routes through the stub
//! `SlotPoolOps::kill_session`, which returns an error — producing the exact
//! `ok: false, error: Some(_)` envelope the test asserts. No real pool needed.

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn execution_kill_task_with_nonexistent_task_returns_error_shape() {
    let harness = McpTestHarness::new().await;

    let response = harness
        .call_tool(
            "execution_kill_task",
            json!({"task_id":"nonexistent-task-id"}),
        )
        .await
        .expect("execution_kill_task should dispatch");

    assert_eq!(response["ok"], false);
    assert!(response.get("error").and_then(|v| v.as_str()).is_some());
}
