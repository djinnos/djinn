#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::db::CredentialRepository;
    use crate::test_helpers::{create_test_app_with_db, create_test_db, initialize_mcp_session, mcp_call_tool};
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn credential_set_success_shape() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
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

        let row: Option<Vec<u8>> = sqlx::query_scalar("SELECT encrypted_value FROM credentials WHERE key_name = ?1")
            .bind("ANTHROPIC_API_KEY")
            .fetch_optional(db.pool())
            .await
            .unwrap();
        let ciphertext = row.expect("missing credential row");
        assert!(!ciphertext.is_empty());
        assert_ne!(ciphertext, b"secret-1");

        let repo = CredentialRepository::new(db.clone(), broadcast::channel(16).0);
        let decrypted = repo.get_decrypted("ANTHROPIC_API_KEY").await.unwrap().unwrap();
        assert_eq!(decrypted, "secret-1");
    }

    #[tokio::test]
    async fn credential_list_hides_secrets() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
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
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
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
