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
    async fn project_settings_validate_reports_valid_and_invalid() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let dir = tempdir().expect("tempdir");
        let djinn = dir.path().join(".djinn");
        std::fs::create_dir_all(&djinn).expect("create .djinn");

        std::fs::write(
            djinn.join("settings.json"),
            r#"{"setup":[{"name":"setup","command":"echo ok"}],"verification":[{"name":"verify","command":"echo ok"}],"extra":true}"#,
        )
        .expect("write settings");

        let valid = mcp_call_tool(
            &app,
            &session_id,
            "project_settings_validate",
            json!({"worktree_path": dir.path().to_string_lossy().to_string()}),
        )
        .await;
        assert_eq!(valid["valid"], true);
        assert!(valid["errors"].as_array().expect("errors").iter().any(|e| {
            e.as_str()
                .unwrap_or_default()
                .contains("warning: unknown top-level key 'extra'")
        }));

        std::fs::write(djinn.join("settings.json"), r#"{"setup":[{"name":"missing-command"}]}"#)
            .expect("write invalid settings");
        let invalid = mcp_call_tool(
            &app,
            &session_id,
            "project_settings_validate",
            json!({"worktree_path": dir.path().to_string_lossy().to_string()}),
        )
        .await;
        assert_eq!(invalid["valid"], false);
        assert!(invalid["errors"].as_array().expect("errors").iter().any(|e| {
            e.as_str()
                .unwrap_or_default()
                .contains("schema validation failed")
        }));
    }

}
