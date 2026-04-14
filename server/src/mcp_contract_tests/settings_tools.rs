use serde_json::json;

use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

#[tokio::test]
async fn settings_get_missing_returns_not_exists() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let res = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
    assert_eq!(res["exists"], false);
    assert!(res["settings"].is_null());
}

#[tokio::test]
async fn settings_set_get_reset_round_trip() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    // Only set dispatch_limit — model_priority requires connected credentials.
    let set = mcp_call_tool(
        &app,
        &session_id,
        "settings_set",
        json!({"dispatch_limit": 7}),
    )
    .await;
    assert_eq!(set["ok"], true);

    let get = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
    assert_eq!(get["exists"], true);
    assert_eq!(get["settings"]["dispatch_limit"], 7);

    let reset = mcp_call_tool(&app, &session_id, "settings_reset", json!({})).await;
    assert_eq!(reset["ok"], true);

    let get2 = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
    assert_eq!(get2["exists"], false);
}

#[tokio::test]
async fn settings_set_rejects_unconnected_model_provider() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    // Validation rejects models referencing providers with no credentials.
    let res = mcp_call_tool(
        &app,
        &session_id,
        "settings_set",
        json!({"models": ["no-such-provider/some-model"]}),
    )
    .await;
    assert_eq!(res["ok"], false);
    assert!(
        res["error"]
            .as_str()
            .unwrap_or_default()
            .contains("disconnected")
    );
}

#[tokio::test]
async fn settings_set_rejects_invalid_memory_mount_configuration() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let res = mcp_call_tool(
        &app,
        &session_id,
        "settings_set",
        json!({
            "memory_mount_enabled": true,
            "memory_mount_path": "relative/memory"
        }),
    )
    .await;

    assert_eq!(res["ok"], false);
    assert!(
        res["error"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid memory mount configuration")
    );
}
