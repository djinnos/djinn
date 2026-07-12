//! Contract tests for image catalog MCP tools.
//!
//! Proves that `lifecycle.pre_task` configs survive the full round-trip:
//! parse/validate → create → list/update → list/get, without dropping or
//! reshaping pre-task command fields.  Also confirms that invalid pre-task
//! configs are rejected through shared `EnvironmentConfig` validation.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use djinn_control_plane::test_support::{McpTestHarness, StubRuntime};
use djinn_db::{Database, ImageRepository};
use serde_json::json;

/// A minimal valid EnvironmentConfig JSON that includes a `lifecycle.pre_task`
/// entry.  Used as the baseline config payload for image create/update tests.
fn config_with_pre_task() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "lifecycle": {
            "pre_task": [
                {
                    "name": "install-deps",
                    "command": "pip install -e .",
                    "timeout_seconds": 120,
                    "failure_policy": "blocking"
                }
            ]
        }
    })
}

/// A minimal valid EnvironmentConfig with two pre-task commands — one with an
/// explicit name and one relying on auto-generated naming.
fn config_with_multi_pre_task() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "lifecycle": {
            "pre_task": [
                {
                    "name": "setup-db",
                    "command": "createdb test",
                    "timeout_seconds": 60,
                    "failure_policy": "best_effort"
                },
                {
                    "command": "npm ci",
                    "timeout_seconds": 300
                }
            ]
        }
    })
}

/// An invalid config: pre-task command is empty — must be rejected.
fn config_with_invalid_pre_task() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "lifecycle": {
            "pre_task": [
                {
                    "name": "bad-cmd",
                    "command": ""
                }
            ]
        }
    })
}

/// An invalid config: pre-task timeout is out of range.
fn config_with_bad_timeout_pre_task() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "lifecycle": {
            "pre_task": [
                {
                    "name": "slow",
                    "command": "echo hi",
                    "timeout_seconds": 0
                }
            ]
        }
    })
}

/// An invalid config: duplicate pre-task names.
fn config_with_duplicate_names() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "lifecycle": {
            "pre_task": [
                {
                    "name": "same",
                    "command": "echo a"
                },
                {
                    "name": "same",
                    "command": "echo b"
                }
            ]
        }
    })
}

// ── parse_validated_config acceptance / rejection ───────────────────────────

#[tokio::test]
async fn image_create_accepts_valid_pre_task_config() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Python-pre-task",
                "config": config_with_pre_task()
            }),
        )
        .await
        .expect("dispatch");

    assert_eq!(
        result["status"], "ok",
        "valid pre-task config must be accepted, got: {result}"
    );
    let id = result["id"].as_str().expect("image id");
    assert!(!id.is_empty());

    // Verify the created image appears in list output with the pre-task fields.
    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");

    let images = listed["images"].as_array().expect("images array");
    let img = images
        .iter()
        .find(|i| i["id"] == json!(id))
        .expect("created image must appear in list");
    assert_eq!(img["status"], "none", "freshly-created image status");

    let config = &img["config"];
    let pre_task = &config["lifecycle"]["pre_task"];
    assert!(
        pre_task.is_array(),
        "config must contain lifecycle.pre_task array"
    );
    let cmds = pre_task.as_array().unwrap();
    assert_eq!(cmds.len(), 1, "expected exactly one pre-task command");
    assert_eq!(cmds[0]["name"], "install-deps");
    assert_eq!(cmds[0]["command"], "pip install -e .");
    assert_eq!(cmds[0]["timeout_seconds"], 120);
    assert_eq!(cmds[0]["failure_policy"], "blocking");
}

#[tokio::test]
async fn image_create_rejects_empty_command() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Bad-pre-task",
                "config": config_with_invalid_pre_task()
            }),
        )
        .await
        .expect("dispatch");

    assert_eq!(
        result["status"], "error",
        "empty command must be rejected, got: {result}"
    );
    let error = result["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("validate"),
        "error must mention validation, got: {error}"
    );
}

#[tokio::test]
async fn image_create_rejects_bad_timeout() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Bad-timeout",
                "config": config_with_bad_timeout_pre_task()
            }),
        )
        .await
        .expect("dispatch");

    assert_eq!(
        result["status"], "error",
        "timeout out of range must be rejected, got: {result}"
    );
}

#[tokio::test]
async fn image_create_rejects_duplicate_names() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Dup-names",
                "config": config_with_duplicate_names()
            }),
        )
        .await
        .expect("dispatch");

    assert_eq!(
        result["status"], "error",
        "duplicate pre-task names must be rejected, got: {result}"
    );
}

// ── create → list round-trip ────────────────────────────────────────────────

#[tokio::test]
async fn image_list_preserves_pre_task_after_create() {
    let harness = McpTestHarness::new().await;

    let created = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Multi-pre-task",
                "description": "tests multi-command pre-task",
                "config": config_with_multi_pre_task()
            }),
        )
        .await
        .expect("dispatch");
    assert_eq!(created["status"], "ok");
    let id = created["id"].as_str().unwrap();

    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");

    let images = listed["images"].as_array().unwrap();
    let img = images
        .iter()
        .find(|i| i["id"] == json!(id))
        .expect("image in list");

    let pre_task = &img["config"]["lifecycle"]["pre_task"];
    let cmds = pre_task.as_array().expect("pre_task array");
    assert_eq!(cmds.len(), 2, "expected two pre-task commands");

    // First command: explicit name and best_effort policy.
    assert_eq!(cmds[0]["name"], "setup-db");
    assert_eq!(cmds[0]["command"], "createdb test");
    assert_eq!(cmds[0]["timeout_seconds"], 60);
    assert_eq!(cmds[0]["failure_policy"], "best_effort");

    // Second command: no explicit name, default timeout, default policy.
    assert_eq!(cmds[1]["command"], "npm ci");
    // Name should be absent or null (serde skip_serializing_if = "Option::is_none")
    assert!(
        cmds[1]["name"].is_null() || !cmds[1].as_object().unwrap().contains_key("name"),
        "auto-generated name should not be serialized, got: {}",
        cmds[1]["name"]
    );
    assert_eq!(cmds[1]["timeout_seconds"], 300);
    // Default failure_policy serializes as "blocking".
    assert_eq!(cmds[1]["failure_policy"], "blocking");

    // Description preserved too.
    assert_eq!(img["description"], "tests multi-command pre-task");
}

// ── update → list/get round-trip ────────────────────────────────────────────

#[tokio::test]
async fn image_update_preserves_pre_task_fields() {
    let harness = McpTestHarness::new().await;

    // Create an image with a simple config (no pre-task).
    let created = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Update-test",
                "config": {
                    "schema_version": 1
                }
            }),
        )
        .await
        .expect("dispatch");
    assert_eq!(created["status"], "ok");
    let id = created["id"].as_str().unwrap().to_string();

    // Verify no pre-task in the initial config.
    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");
    let images = listed["images"].as_array().unwrap();
    let img = images
        .iter()
        .find(|i| i["id"] == json!(id))
        .expect("image in list");
    let initial_pre_task = &img["config"]["lifecycle"]["pre_task"];
    assert!(
        initial_pre_task.is_null() || initial_pre_task.as_array().is_none_or(|a| a.is_empty()),
        "initial config should have no pre-task commands"
    );

    // Update the image with a config that has pre-task entries.
    let updated = harness
        .call_tool(
            "image_update",
            json!({
                "id": id,
                "name": "Update-test",
                "config": config_with_multi_pre_task()
            }),
        )
        .await
        .expect("dispatch");
    assert_eq!(updated["status"], "ok", "update must succeed: {updated}");

    // Verify pre-task survives in list output after update.
    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");
    let images = listed["images"].as_array().unwrap();
    let img = images
        .iter()
        .find(|i| i["id"] == json!(id))
        .expect("image in list after update");

    let pre_task = &img["config"]["lifecycle"]["pre_task"];
    let cmds = pre_task.as_array().expect("pre_task array after update");
    assert_eq!(cmds.len(), 2, "two pre-task commands after update");
    assert_eq!(cmds[0]["name"], "setup-db");
    assert_eq!(cmds[0]["command"], "createdb test");
    assert_eq!(cmds[0]["failure_policy"], "best_effort");
    assert_eq!(cmds[1]["command"], "npm ci");

    // Build state must have been reset by the update.
    assert_eq!(img["status"], "none", "update resets status to none");
}

#[tokio::test]
async fn image_update_with_pre_task_overwrites_previous_pre_task() {
    let harness = McpTestHarness::new().await;

    // Create with one pre-task command.
    let created = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Overwrite-test",
                "config": config_with_pre_task()
            }),
        )
        .await
        .expect("dispatch");
    let id = created["id"].as_str().unwrap().to_string();

    // Verify initial pre-task.
    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");
    let img = listed["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id))
        .unwrap()
        .clone();
    let initial_cmds = img["config"]["lifecycle"]["pre_task"].as_array().unwrap();
    assert_eq!(initial_cmds.len(), 1);
    assert_eq!(initial_cmds[0]["name"], "install-deps");

    // Update to a different set of pre-task commands.
    let updated = harness
        .call_tool(
            "image_update",
            json!({
                "id": id,
                "name": "Overwrite-test",
                "config": json!({
                    "schema_version": 1,
                    "lifecycle": {
                        "pre_task": [
                            {
                                "name": "migrate",
                                "command": "python manage.py migrate",
                                "timeout_seconds": 180,
                                "failure_policy": "best_effort"
                            }
                        ]
                    }
                })
            }),
        )
        .await
        .expect("dispatch");
    assert_eq!(updated["status"], "ok");

    // Verify the old pre-task is gone and the new one is present.
    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");
    let img = listed["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id))
        .unwrap()
        .clone();
    let cmds = img["config"]["lifecycle"]["pre_task"].as_array().unwrap();
    assert_eq!(cmds.len(), 1, "old pre-task must be replaced");
    assert_eq!(cmds[0]["name"], "migrate");
    assert_eq!(cmds[0]["command"], "python manage.py migrate");
    assert_eq!(cmds[0]["timeout_seconds"], 180);
    assert_eq!(cmds[0]["failure_policy"], "best_effort");
}

// ── Service presets remain separate ─────────────────────────────────────────

#[tokio::test]
async fn service_presets_independent_of_pre_task() {
    let harness = McpTestHarness::new().await;

    // Create image with pre-task.
    let created = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Preset-test",
                "config": config_with_pre_task()
            }),
        )
        .await
        .expect("dispatch");
    let id = created["id"].as_str().unwrap().to_string();

    // Service presets start empty.
    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");
    let img = listed["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id))
        .unwrap()
        .clone();
    assert!(
        img["service_presets"].as_array().unwrap().is_empty(),
        "no service presets initially"
    );

    // Pre-task is still present and independent.
    let pre_task = img["config"]["lifecycle"]["pre_task"].as_array().unwrap();
    assert_eq!(pre_task.len(), 1);
    assert_eq!(pre_task[0]["name"], "install-deps");
}

// ── Absent lifecycle / pre_task defaults to empty ───────────────────────────

#[tokio::test]
async fn config_without_lifecycle_defaults_to_empty_pre_task() {
    let harness = McpTestHarness::new().await;

    // Create with a config that has no lifecycle key at all.
    let created = harness
        .call_tool(
            "image_create",
            json!({
                "name": "No-lifecycle",
                "config": {
                    "schema_version": 1,
                    "system_packages": ["curl"]
                }
            }),
        )
        .await
        .expect("dispatch");
    assert_eq!(created["status"], "ok");
    let id = created["id"].as_str().unwrap();

    let listed = harness
        .call_tool("image_list", json!({}))
        .await
        .expect("image_list");
    let img = listed["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == json!(id))
        .unwrap()
        .clone();

    // lifecycle should be present (serde default) with empty pre_task.
    let pre_task = &img["config"]["lifecycle"]["pre_task"];
    assert!(
        pre_task.is_array() && pre_task.as_array().unwrap().is_empty(),
        "absent lifecycle must deserialize to empty pre_task, got: {pre_task}"
    );
    // system_packages still present.
    assert_eq!(img["config"]["system_packages"], json!(["curl"]));
}

#[tokio::test]
async fn project_set_image_keeps_success_when_build_enqueue_is_deferred() {
    let db = Database::open_in_memory().expect("open test database");
    let runtime =
        Arc::new(StubRuntime::default().with_image_enqueue_error("synthetic controller outage"));
    let harness = McpTestHarness::from_db_with_runtime(db.clone(), runtime);
    let project = common::create_test_project(&db).await;

    let created = harness
        .call_tool(
            "image_create",
            json!({
                "name": "Deferred-build",
                "config": { "schema_version": 1 }
            }),
        )
        .await
        .expect("image_create");
    assert_eq!(created["status"], "ok");
    let image_id = created["id"].as_str().expect("image id");

    let assigned = harness
        .call_tool(
            "project_set_image",
            json!({ "project": project.id.clone(), "image_id": image_id }),
        )
        .await
        .expect("project_set_image");
    assert_eq!(
        assigned["status"], "ok",
        "durable assignment must not be reported as failed: {assigned}"
    );

    let selected = ImageRepository::new(db)
        .resolve_for_project(&project.id)
        .await
        .expect("resolve assignment")
        .expect("assigned image");
    assert_eq!(selected.id, image_id);
}
