use serde_json::json;

use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

#[tokio::test]
async fn execution_kill_task_with_nonexistent_task_returns_error_shape() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let response = mcp_call_tool(
        &app,
        &session_id,
        "execution_kill_task",
        json!({"task_id":"nonexistent-task-id"}),
    )
    .await;

    assert_eq!(response["ok"], false);
    assert!(response.get("error").and_then(|v| v.as_str()).is_some());
}
