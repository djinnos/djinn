use serde_json::json;

use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

#[tokio::test]
async fn system_ping_returns_version() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let res = mcp_call_tool(&app, &session_id, "system_ping", json!({})).await;
    assert_eq!(res["status"], "ok");
    assert_eq!(res["version"], env!("CARGO_PKG_VERSION"));
}
