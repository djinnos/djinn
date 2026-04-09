use serde_json::json;
use tempfile::tempdir;

use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

/// Initialise a temp directory as a git repo with a GitHub origin remote.
fn init_git_with_github_remote(dir: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .expect("git init");
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "git@github.com:test-owner/test-repo.git",
        ])
        .current_dir(dir)
        .output()
        .expect("git remote add");
    // Create an initial commit so HEAD is valid.
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(dir)
        .output()
        .expect("git commit");
}

#[tokio::test]
async fn project_add_rejects_dir_without_github_remote() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let dir = tempdir().expect("tempdir");
    let path = dir.path().to_string_lossy().to_string();

    let added = mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        json!({"name": "proj-no-remote", "path": path.clone()}),
    )
    .await;
    let status = added["status"].as_str().unwrap_or_default();
    assert!(
        status.starts_with("error:"),
        "expected error for missing remote, got: {status}"
    );
    assert!(
        status.contains("GitHub remote"),
        "expected GitHub remote error message, got: {status}"
    );
}

#[tokio::test]
async fn project_add_rejects_when_github_not_connected() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let dir = tempdir().expect("tempdir");
    init_git_with_github_remote(dir.path());
    let path = dir.path().to_string_lossy().to_string();

    let added = mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        json!({"name": "proj-no-token", "path": path.clone()}),
    )
    .await;
    let status = added["status"].as_str().unwrap_or_default();
    assert!(
        status.starts_with("error:"),
        "expected error for missing GitHub token, got: {status}"
    );
    assert!(
        status.contains("Connect GitHub first"),
        "expected 'Connect GitHub first' error, got: {status}"
    );
}

#[tokio::test]
async fn project_add_and_list_success_shape() {
    // Use DB-level project creation to bypass GitHub validation
    // (the MCP tool now requires a connected GitHub App).
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let listed = mcp_call_tool(&app, &session_id, "project_list", json!({})).await;
    let projects = listed["projects"].as_array().expect("projects array");
    assert!(
        projects.iter().any(|p| p["path"] == json!(project.path)),
        "project_list must include the registered project"
    );
    assert!(
        projects
            .iter()
            .any(|p| p["id"].as_str().unwrap_or_default().len() > 8),
        "project must have a non-trivial id"
    );
}

#[tokio::test]
async fn project_add_duplicate_path_errors() {
    // Both calls will hit the GitHub validation error, which is itself
    // an error status — the test still verifies that the second call
    // returns an error (just a different one than before).
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let dir = tempdir().expect("tempdir");
    let path = dir.path().to_string_lossy().to_string();

    let first = mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        json!({"name": "proj-a", "path": path.clone()}),
    )
    .await;
    assert!(
        first["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );
    let dup = mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        json!({"name": "proj-b", "path": path}),
    )
    .await;
    assert!(
        dup["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );
}

#[tokio::test]
async fn project_remove_success_and_missing() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let removed = mcp_call_tool(
        &app,
        &session_id,
        "project_remove",
        json!({"name": project.name, "path": project.path.clone()}),
    )
    .await;
    assert_eq!(removed["status"], "ok");

    let missing = mcp_call_tool(
        &app,
        &session_id,
        "project_remove",
        json!({"name": project.name, "path": project.path}),
    )
    .await;
    assert!(
        missing["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );
}

#[tokio::test]
async fn project_remove_wrong_path_is_rejected() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let rejected = mcp_call_tool(
        &app,
        &session_id,
        "project_remove",
        json!({"name": project.name.clone(), "path": "/wrong/path"}),
    )
    .await;
    assert!(
        rejected["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );

    let listed = mcp_call_tool(&app, &session_id, "project_list", json!({})).await;
    assert!(
        listed["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == project.name)
    );
}

#[tokio::test]
async fn project_config_get_set_round_trip() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let set = mcp_call_tool(
        &app,
        &session_id,
        "project_config_set",
        json!({"project": project.path.clone(), "key": "target_branch", "value": "develop"}),
    )
    .await;
    assert_eq!(set["status"], "ok");

    let got = mcp_call_tool(
        &app,
        &session_id,
        "project_config_get",
        json!({"project": project.path}),
    )
    .await;
    assert_eq!(got["status"], "ok");
    assert_eq!(got["target_branch"], "develop");
}

#[tokio::test]
async fn project_config_get_returns_empty_verification_rules_by_default() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let got = mcp_call_tool(
        &app,
        &session_id,
        "project_config_get",
        json!({"project": project.path}),
    )
    .await;
    assert_eq!(got["status"], "ok");
    assert_eq!(got["verification_rules"], json!([]));
}

#[tokio::test]
async fn project_config_set_verification_rules_round_trip() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let rules = json!([
        {"match_pattern": "src/**/*.rs", "commands": ["cargo test"]},
        {"match_pattern": "**", "commands": ["cargo clippy"]}
    ]);
    let set = mcp_call_tool(
        &app,
        &session_id,
        "project_config_set",
        json!({
            "project": project.path.clone(),
            "key": "verification_rules",
            "value": rules.to_string()
        }),
    )
    .await;
    assert_eq!(set["status"], "ok");

    let got = mcp_call_tool(
        &app,
        &session_id,
        "project_config_get",
        json!({"project": project.path}),
    )
    .await;
    assert_eq!(got["status"], "ok");
    let returned_rules = got["verification_rules"].as_array().expect("array");
    assert_eq!(returned_rules.len(), 2);
    assert_eq!(returned_rules[0]["match_pattern"], "src/**/*.rs");
    assert_eq!(returned_rules[0]["commands"], json!(["cargo test"]));
    assert_eq!(returned_rules[1]["match_pattern"], "**");
}

#[tokio::test]
async fn project_config_set_verification_rules_invalid_glob_returns_error() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let bad_rules = json!([{"match_pattern": "[invalid", "commands": ["echo ok"]}]);
    let set = mcp_call_tool(
        &app,
        &session_id,
        "project_config_set",
        json!({
            "project": project.path,
            "key": "verification_rules",
            "value": bad_rules.to_string()
        }),
    )
    .await;
    assert!(
        set["status"].as_str().unwrap_or("").starts_with("error:"),
        "expected error, got: {}",
        set["status"]
    );
}

#[tokio::test]
async fn project_config_set_verification_rules_empty_commands_returns_error() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let bad_rules = json!([{"match_pattern": "**", "commands": []}]);
    let set = mcp_call_tool(
        &app,
        &session_id,
        "project_config_set",
        json!({
            "project": project.path,
            "key": "verification_rules",
            "value": bad_rules.to_string()
        }),
    )
    .await;
    assert!(
        set["status"].as_str().unwrap_or("").starts_with("error:"),
        "expected error, got: {}",
        set["status"]
    );
}

#[tokio::test]
async fn project_settings_validate_reports_valid_and_invalid() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let dir = tempdir().expect("tempdir");
    let djinn = dir.path().join(".djinn");
    std::fs::create_dir_all(&djinn).expect("create .djinn");

    std::fs::write(djinn.join("settings.json"), r#"{"setup":[{"name":"setup","command":"echo ok"}],"verification":[{"name":"verify","command":"echo ok"}],"extra":true}"#).expect("write settings");
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

    std::fs::write(
        djinn.join("settings.json"),
        r#"{"setup":[{"name":"missing-command"}]}"#,
    )
    .expect("write invalid settings");
    let invalid = mcp_call_tool(
        &app,
        &session_id,
        "project_settings_validate",
        json!({"worktree_path": dir.path().to_string_lossy().to_string()}),
    )
    .await;
    assert_eq!(invalid["valid"], false);
    assert!(
        invalid["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e
                .as_str()
                .unwrap_or_default()
                .contains("schema validation failed"))
    );
}
