#[cfg(test)]
mod tests {
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

        let set = mcp_call_tool(
            &app,
            &session_id,
            "settings_set",
            json!({"dispatch_limit": 7, "model_priority_worker": ["chatgpt_codex/gpt-5.3-codex"]}),
        )
        .await;
        assert_eq!(set["ok"], true);

        let get = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
        assert_eq!(get["exists"], true);
        assert_eq!(get["settings"]["dispatch_limit"], 7);
        assert_eq!(
            get["settings"]["model_priority"]["worker"][0],
            "chatgpt_codex/gpt-5.3-codex"
        );

        let reset = mcp_call_tool(&app, &session_id, "settings_reset", json!({})).await;
        assert_eq!(reset["ok"], true);

        let get2 = mcp_call_tool(&app, &session_id, "settings_get", json!({})).await;
        assert_eq!(get2["exists"], false);
    }

    #[tokio::test]
    async fn settings_set_rejects_bad_model_priority_format() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let res = mcp_call_tool(
            &app,
            &session_id,
            "settings_set",
            json!({"model_priority_worker": ["invalid-format"]}),
        )
        .await;

        assert_eq!(res["ok"], false);
        assert!(res["error"].as_str().unwrap_or_default().contains("provider/model"));
    }
}
