use serde_json::json;

use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

#[tokio::test]
async fn execution_start_without_pool_or_coordinator_returns_error_shape() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let response = mcp_call_tool(&app, &session_id, "execution_start", json!({})).await;

    assert_eq!(response["ok"], false);
    assert!(response.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn execution_status_without_pool_or_coordinator_returns_error_shape() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let response = mcp_call_tool(&app, &session_id, "execution_status", json!({})).await;

    assert_eq!(response["ok"], false);
    assert!(response.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn execution_pause_and_resume_without_pool_or_coordinator_return_error_shapes() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let pause = mcp_call_tool(
        &app,
        &session_id,
        "execution_pause",
        json!({"mode":"graceful"}),
    )
    .await;
    assert_eq!(pause["ok"], false);
    assert!(pause.get("error").and_then(|v| v.as_str()).is_some());

    let resume = mcp_call_tool(&app, &session_id, "execution_resume", json!({})).await;
    assert_eq!(resume["ok"], false);
    assert!(resume.get("error").and_then(|v| v.as_str()).is_some());
}

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
