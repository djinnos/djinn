//! Contract tests for `project_*` MCP tools.
//!
//! Migrated from `server/src/mcp_contract_tests/project_tools.rs`.  The
//! ignored `project_remove_success_and_missing` test stays in the server crate
//! because it tracks a Dolt multi-cascade DELETE bug unrelated to the harness.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn project_add_rejects_empty_owner_or_repo() {
    // `project_add_from_github` validates that owner and repo are non-empty
    // before anything else — exercising that fast-path doesn't need any GitHub
    // session.
    let harness = McpTestHarness::new().await;

    let added = harness
        .call_tool(
            "project_add_from_github",
            json!({"owner": "", "repo": ""}),
        )
        .await
        .expect("project_add_from_github should dispatch");
    let status = added["status"].as_str().unwrap_or_default();
    assert!(
        status.starts_with("error:"),
        "expected error for empty owner/repo, got: {status}"
    );
    assert!(
        status.contains("owner and repo must be non-empty"),
        "expected empty-owner/repo error message, got: {status}"
    );
}

#[tokio::test]
async fn project_add_rejects_when_github_not_connected() {
    // Without a GitHub-authenticated session the server must refuse to clone.
    let harness = McpTestHarness::new().await;

    let added = harness
        .call_tool(
            "project_add_from_github",
            json!({"owner": "test-owner", "repo": "test-repo"}),
        )
        .await
        .expect("project_add_from_github should dispatch");
    let status = added["status"].as_str().unwrap_or_default();
    assert!(
        status.starts_with("error:"),
        "expected error for missing GitHub token, got: {status}"
    );
    assert!(
        status.contains("sign in with GitHub required"),
        "expected 'sign in with GitHub required' error, got: {status}"
    );
}

#[tokio::test]
async fn project_add_and_list_success_shape() {
    // Use DB-level project creation to bypass GitHub validation (the MCP tool
    // now requires a connected GitHub App).
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    let listed = harness
        .call_tool("project_list", json!({}))
        .await
        .expect("project_list should dispatch");
    let projects = listed["projects"].as_array().expect("projects array");
    assert!(
        projects.iter().any(|p| p["github_owner"] == json!(project.github_owner)
            && p["github_repo"] == json!(project.github_repo)),
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
    // Under `project_add_from_github` the clone path is derived from
    // `{owner}/{repo}`, so "duplicate path" manifests as two calls for the
    // same repo.  Without a GitHub-authenticated session both calls short
    // circuit on token validation — the test still verifies that both
    // return an error status (rather than a success shape) for the same
    // inputs.
    let harness = McpTestHarness::new().await;

    let first = harness
        .call_tool(
            "project_add_from_github",
            json!({"owner": "test-owner", "repo": "test-repo"}),
        )
        .await
        .expect("project_add_from_github should dispatch");
    assert!(
        first["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );
    let dup = harness
        .call_tool(
            "project_add_from_github",
            json!({"owner": "test-owner", "repo": "test-repo"}),
        )
        .await
        .expect("project_add_from_github should dispatch");
    assert!(
        dup["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );
}

#[tokio::test]
async fn project_remove_unknown_slug_is_rejected() {
    // `project_remove` now takes a single `project` handle (UUID or
    // `owner/repo` slug). A non-existent slug must be refused without
    // disturbing already-registered projects.
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    let rejected = harness
        .call_tool(
            "project_remove",
            json!({"project": "nonexistent/project"}),
        )
        .await
        .expect("project_remove should dispatch");
    assert!(
        rejected["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );

    let listed = harness
        .call_tool("project_list", json!({}))
        .await
        .expect("project_list should dispatch");
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
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    let set = harness
        .call_tool(
            "project_config_set",
            json!({"project": project.slug(), "key": "target_branch", "value": "develop"}),
        )
        .await
        .expect("project_config_set should dispatch");
    assert_eq!(set["status"], "ok");

    let got = harness
        .call_tool("project_config_get", json!({"project": project.slug()}))
        .await
        .expect("project_config_get should dispatch");
    assert_eq!(got["status"], "ok");
    assert_eq!(got["target_branch"], "develop");
}

#[tokio::test]
async fn project_graph_exclusions_set_and_get_round_trip() {
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    let set = harness
        .call_tool(
            "project_graph_exclusions_set",
            json!({
                "project": project.slug(),
                "graph_excluded_paths": ["**/workspace-hack/**", "**/target/**"],
                "graph_orphan_ignore": ["crates/test-support/src/fixtures.rs"],
            }),
        )
        .await
        .expect("project_graph_exclusions_set should dispatch");
    assert_eq!(set["status"], "ok");
    let paths = set["graph_excluded_paths"].as_array().expect("paths array");
    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(|p| p == "**/workspace-hack/**"));

    let got = harness
        .call_tool(
            "project_graph_exclusions_get",
            json!({"project": project.slug()}),
        )
        .await
        .expect("project_graph_exclusions_get should dispatch");
    assert_eq!(got["status"], "ok");
    assert_eq!(got["graph_excluded_paths"].as_array().unwrap().len(), 2);
    assert_eq!(got["graph_orphan_ignore"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn project_graph_exclusions_partial_update_leaves_other_field_untouched() {
    // `graph_excluded_paths: None` + `graph_orphan_ignore: Some(..)` must
    // NOT clobber the previously-set paths. Validates the partial-update
    // contract — we issue two updates and check both survive.
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    // Seed both.
    let _ = harness
        .call_tool(
            "project_graph_exclusions_set",
            json!({
                "project": project.slug(),
                "graph_excluded_paths": ["**/vendor/**"],
                "graph_orphan_ignore": ["a.rs"],
            }),
        )
        .await
        .expect("initial set");

    // Now update only the orphan list.
    let set = harness
        .call_tool(
            "project_graph_exclusions_set",
            json!({
                "project": project.slug(),
                "graph_orphan_ignore": ["a.rs", "b.rs"],
            }),
        )
        .await
        .expect("partial update");
    assert_eq!(set["status"], "ok");
    // `**/vendor/**` must still be there.
    let paths = set["graph_excluded_paths"].as_array().expect("paths");
    assert!(
        paths.iter().any(|p| p == "**/vendor/**"),
        "partial update erased graph_excluded_paths: {paths:?}"
    );
    assert_eq!(set["graph_orphan_ignore"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn project_graph_exclusions_get_missing_project_errors() {
    let harness = McpTestHarness::new().await;

    let got = harness
        .call_tool(
            "project_graph_exclusions_get",
            json!({"project": "/nonexistent/path"}),
        )
        .await
        .expect("dispatch should succeed");
    assert!(
        got["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error: project not found"),
        "expected 'project not found' error, got {:?}",
        got["status"]
    );
}
