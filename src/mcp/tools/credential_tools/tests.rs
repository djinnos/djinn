#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

    #[tokio::test]
    async fn credential_set_success_shape() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let res = mcp_call_tool(
            &app,
            &session_id,
            "credential_set",
            json!({"provider_id":"anthropic","key_name":"ANTHROPIC_API_KEY","api_key":"secret-1"}),
        )
        .await;

        assert_eq!(res["ok"], true);
        assert_eq!(res["success"], true);
        assert_eq!(res["key_name"], "ANTHROPIC_API_KEY");
        assert!(res["id"].as_str().unwrap_or_default().len() > 8);
    }

    #[tokio::test]
    async fn credential_list_hides_secrets() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "credential_set",
            json!({"provider_id":"openai","key_name":"OPENAI_API_KEY","api_key":"super-secret"}),
        )
        .await;

        let list = mcp_call_tool(&app, &session_id, "credential_list", json!({})).await;
        let first = list["credentials"].as_array().unwrap().first().unwrap();
        assert_eq!(first["key_name"], "OPENAI_API_KEY");
        assert!(first.get("api_key").is_none());
        assert!(first.get("ciphertext").is_none());
    }

    #[tokio::test]
    async fn credential_delete_removes_credential() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "credential_set",
            json!({"provider_id":"openai","key_name":"OPENAI_API_KEY","api_key":"a"}),
        )
        .await;

        let deleted = mcp_call_tool(
            &app,
            &session_id,
            "credential_delete",
            json!({"key_name":"OPENAI_API_KEY"}),
        )
        .await;
        assert_eq!(deleted["ok"], true);
        assert_eq!(deleted["deleted"], true);
    }
}
