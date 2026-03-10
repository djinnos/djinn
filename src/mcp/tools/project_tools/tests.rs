#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

    #[tokio::test]
    async fn project_add_and_list_success_shape() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");

        let added = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-a", "path": dir.path().to_string_lossy().to_string()}),
        )
        .await;

        assert_eq!(added["status"], "ok");
        assert!(added["project"]["id"].as_str().unwrap_or_default().len() > 8);
        assert_eq!(
            added["project"]["path"],
            json!(dir.path().to_string_lossy().to_string())
        );

        let listed = mcp_call_tool(&app, &session_id, "project_list", json!({})).await;
        let projects = listed["projects"].as_array().expect("projects array");
        assert!(projects
            .iter()
            .any(|p| p["path"] == json!(dir.path().to_string_lossy().to_string())));
    }

    #[tokio::test]
    async fn project_add_duplicate_path_errors() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().to_string();

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-a", "path": path}),
        )
        .await;

        let duplicate = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-b", "path": dir.path().to_string_lossy().to_string()}),
        )
        .await;

        assert!(duplicate["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:"));
    }

    #[tokio::test]
    async fn project_remove_success_and_missing() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-remove", "path": dir.path().to_string_lossy().to_string()}),
        )
        .await;

        let removed = mcp_call_tool(
            &app,
            &session_id,
            "project_remove",
            json!({"name": "proj-remove"}),
        )
        .await;
        assert_eq!(removed["status"], "ok");

        let missing = mcp_call_tool(
            &app,
            &session_id,
            "project_remove",
            json!({"name": "proj-remove"}),
        )
        .await;
        assert!(missing["status"].as_str().unwrap_or_default().starts_with("error:"));
    }

    #[tokio::test]
    async fn project_config_get_set_round_trip() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().to_string();

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-config", "path": path}),
        )
        .await;

        let set = mcp_call_tool(
            &app,
            &session_id,
            "project_config_set",
            json!({"project": dir.path().to_string_lossy().to_string(), "key": "target_branch", "value": "develop"}),
        )
        .await;
        assert_eq!(set["status"], "ok");

        let got = mcp_call_tool(
            &app,
            &session_id,
            "project_config_get",
            json!({"project": dir.path().to_string_lossy().to_string()}),
        )
        .await;
        assert_eq!(got["status"], "ok");
        assert_eq!(got["target_branch"], "develop");
    }

    #[tokio::test]
    async fn project_commands_get_set_round_trip() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            json!({"name": "proj-cmd", "path": dir.path().to_string_lossy().to_string()}),
        )
        .await;

        let set = mcp_call_tool(
            &app,
            &session_id,
            "project_commands_set",
            json!({
                "project": dir.path().to_string_lossy().to_string(),
                "setup_commands": [{"name": "setup", "command": "echo ok", "timeout_secs": 10}],
                "verification_commands": [{"name": "verify", "command": "echo pass", "timeout_secs": 10}]
            }),
        )
        .await;
        assert_eq!(set["status"], "ok");

        let got = mcp_call_tool(
            &app,
            &session_id,
            "project_commands_get",
            json!({"project": dir.path().to_string_lossy().to_string()}),
        )
        .await;
        assert_eq!(got["status"], "ok");
        assert_eq!(got["setup_commands"][0]["name"], "setup");
        assert_eq!(got["verification_commands"][0]["name"], "verify");
    }
}
