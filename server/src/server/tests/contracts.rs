use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::helpers::CONTRACT_PROJECT_PATH;
use crate::server::{self, AppState};
use crate::test_helpers;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_contract_desktop_critical_tools_success_shapes() {
    let (app, project_path, _project_dir) = test_helpers::create_test_app_with_project().await;
    let session_id = test_helpers::initialize_mcp_session(&app).await;

    let provider_catalog =
        test_helpers::mcp_call_tool(&app, &session_id, "provider_catalog", serde_json::json!({}))
            .await;
    let providers = provider_catalog
        .get("providers")
        .and_then(Value::as_array)
        .expect("provider_catalog must return providers array");
    assert!(
        !providers.is_empty(),
        "provider_catalog providers should not be empty"
    );
    for provider in providers {
        assert!(provider.get("id").and_then(Value::as_str).is_some());
        assert!(provider.get("name").and_then(Value::as_str).is_some());
        assert!(provider.get("connected").and_then(Value::as_bool).is_some());
    }

    let credential_list =
        test_helpers::mcp_call_tool(&app, &session_id, "credential_list", serde_json::json!({}))
            .await;
    assert!(
        credential_list
            .get("credentials")
            .and_then(Value::as_array)
            .is_some(),
        "credential_list must return credentials array"
    );

    let task_list = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_list",
        serde_json::json!({ "project": project_path }),
    )
    .await;
    assert!(task_list.get("tasks").and_then(Value::as_array).is_some());
    assert!(
        task_list
            .get("total_count")
            .and_then(Value::as_i64)
            .is_some()
    );
    assert!(task_list.get("limit").and_then(Value::as_i64).is_some());
    assert!(task_list.get("offset").and_then(Value::as_i64).is_some());
    assert!(task_list.get("has_more").and_then(Value::as_bool).is_some());

    let epic_list = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "epic_list",
        serde_json::json!({ "project": project_path }),
    )
    .await;
    assert!(epic_list.get("epics").and_then(Value::as_array).is_some());
    assert!(
        epic_list
            .get("total_count")
            .and_then(Value::as_i64)
            .is_some()
    );
    assert!(epic_list.get("limit").and_then(Value::as_i64).is_some());
    assert!(epic_list.get("offset").and_then(Value::as_i64).is_some());
    assert!(epic_list.get("has_more").and_then(Value::as_bool).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_contract_task_and_epic_snapshot_shapes() {
    use insta::assert_json_snapshot;

    let (app, project_path, _project_dir) = test_helpers::create_test_app_with_project().await;
    let session_id = test_helpers::initialize_mcp_session(&app).await;

    let epic = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "epic_create",
        serde_json::json!({
            "project": project_path,
            "title": "Snapshot Epic",
            "description": "Epic used for MCP snapshot contract testing"
        }),
    )
    .await;

    let task = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_create",
        serde_json::json!({
            "project": project_path,
            "epic_id": epic["id"],
            "title": "Snapshot Task",
            "description": "Task used for MCP snapshot contract testing"
        }),
    )
    .await;

    let task_show = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_show",
        serde_json::json!({
            "project": project_path,
            "id": task["id"],
        }),
    )
    .await;
    assert_json_snapshot!("task_show_response", task_show, {
        ".id" => "[UUID]",
        ".epic_id" => "[UUID]",
        ".project_id" => "[UUID]",
        ".short_id" => "[SHORT_ID]",
        ".created_at" => "[TIMESTAMP]",
        ".updated_at" => "[TIMESTAMP]"
    });

    let task_list = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_list",
        serde_json::json!({ "project": project_path, "limit": 10, "offset": 0 }),
    )
    .await;
    assert_json_snapshot!("task_list_response", task_list, {
        ".tasks.**.id" => "[UUID]",
        ".tasks.**.epic_id" => "[UUID]",
        ".tasks.**.project_id" => "[UUID]",
        ".tasks.**.short_id" => "[SHORT_ID]",
        ".tasks.**.created_at" => "[TIMESTAMP]",
        ".tasks.**.updated_at" => "[TIMESTAMP]"
    });

    let task_count_plain = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_count",
        serde_json::json!({ "project": project_path }),
    )
    .await;
    assert_json_snapshot!("task_count_plain_response", task_count_plain);

    let task_count_status = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_count",
        serde_json::json!({ "project": project_path, "group_by": "status" }),
    )
    .await;
    assert_json_snapshot!("task_count_grouped_by_status_response", task_count_status);

    let task_count_priority = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_count",
        serde_json::json!({ "project": project_path, "group_by": "priority" }),
    )
    .await;
    assert_json_snapshot!(
        "task_count_grouped_by_priority_response",
        task_count_priority
    );

    let _comment = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_comment_add",
        serde_json::json!({
            "project": project_path,
            "id": task["id"],
            "actor_id": "u1",
            "actor_role": "user",
            "body": "snapshot comment"
        }),
    )
    .await;

    let task_activity = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_activity_list",
        serde_json::json!({ "project": project_path, "id": task["id"] }),
    )
    .await;
    assert_json_snapshot!("task_activity_list_response", task_activity, {
        ".entries.**.id" => "[UUID]",
        ".entries.**.task_id" => "[UUID]",
        ".entries.**.actor_id" => "[UUID]",
        ".entries.**.created_at" => "[TIMESTAMP]",
        ".entries.**.timestamp" => "[TIMESTAMP]"
    });

    let epic_show = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "epic_show",
        serde_json::json!({ "project": project_path, "id": epic["id"] }),
    )
    .await;
    assert_json_snapshot!("epic_show_response", epic_show, {
        ".id" => "[UUID]",
        ".project_id" => "[UUID]",
        ".short_id" => "[SHORT_ID]",
        ".created_at" => "[TIMESTAMP]",
        ".updated_at" => "[TIMESTAMP]"
    });

    let epic_list = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "epic_list",
        serde_json::json!({ "project": project_path, "limit": 10, "offset": 0 }),
    )
    .await;
    assert_json_snapshot!("epic_list_response", epic_list, {
        ".epics.**.id" => "[UUID]",
        ".epics.**.project_id" => "[UUID]",
        ".epics.**.short_id" => "[SHORT_ID]",
        ".epics.**.created_at" => "[TIMESTAMP]",
        ".epics.**.updated_at" => "[TIMESTAMP]"
    });

    let blockers = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_blockers_list",
        serde_json::json!({ "project": project_path, "id": task["id"] }),
    )
    .await;
    assert_json_snapshot!("task_blockers_list_response", blockers, {
        ".blockers.**.id" => "[UUID]",
        ".blockers.**.epic_id" => "[UUID]",
        ".blockers.**.project_id" => "[UUID]",
        ".blockers.**.created_at" => "[TIMESTAMP]",
        ".blockers.**.updated_at" => "[TIMESTAMP]"
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_contract_not_found_shapes_include_error_field() {
    let app = test_helpers::create_test_app();
    let session_id = test_helpers::initialize_mcp_session(&app).await;

    let _ = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        serde_json::json!({
            "name": "contract-not-found-project",
            "path": CONTRACT_PROJECT_PATH,
        }),
    )
    .await;

    let task_show = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "task_show",
        serde_json::json!({
            "project": CONTRACT_PROJECT_PATH,
            "id": "task-does-not-exist",
        }),
    )
    .await;
    assert!(
        task_show.get("error").and_then(Value::as_str).is_some(),
        "task_show not-found response must include error"
    );

    let epic_show = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "epic_show",
        serde_json::json!({
            "project": CONTRACT_PROJECT_PATH,
            "id": "epic-does-not-exist",
        }),
    )
    .await;
    assert!(
        epic_show.get("error").and_then(Value::as_str).is_some(),
        "epic_show not-found response must include error"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_contract_board_health_empty_board_returns_zero_counts() {
    let app = test_helpers::create_test_app();
    let session_id = test_helpers::initialize_mcp_session(&app).await;

    let _ = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        serde_json::json!({
            "name": "contract-board-health-empty",
            "path": CONTRACT_PROJECT_PATH,
        }),
    )
    .await;

    let health = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "board_health",
        serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
    )
    .await;

    assert_eq!(
        health["stale_tasks"]
            .as_array()
            .map(|v| v.len())
            .unwrap_or_default(),
        0
    );
    assert_eq!(
        health["epic_stats"]
            .as_array()
            .map(|v| v.len())
            .unwrap_or_default(),
        0
    );
    assert_eq!(
        health["review_queue"]
            .as_array()
            .map(|v| v.len())
            .unwrap_or_default(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_contract_board_health_response_shape_has_required_fields() {
    let (app, project_path, _project_dir) = test_helpers::create_test_app_with_project().await;
    let session_id = test_helpers::initialize_mcp_session(&app).await;

    let health = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "board_health",
        serde_json::json!({ "project": project_path }),
    )
    .await;

    assert!(health.get("stale_tasks").is_some());
    assert!(health.get("epic_stats").is_some());
    assert!(health.get("review_queue").is_some());
    assert!(health.get("stale_threshold_hours").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_contract_board_health_detects_stale_in_progress_task() {
    let db = test_helpers::create_test_db();
    let cancel = CancellationToken::new();
    let state = AppState::new(db.clone(), cancel);
    let app = server::router(state, false);
    let session_id = test_helpers::initialize_mcp_session(&app).await;

    let _ = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        serde_json::json!({
            "name": "contract-board-health-stale",
            "path": CONTRACT_PROJECT_PATH,
        }),
    )
    .await;

    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let repo = djinn_db::TaskRepository::new(db.clone(), crate::events::EventBus::noop());
    repo.set_status(&task.id, "in_progress").await.unwrap();
    repo.set_updated_at(&task.id, "2020-01-01T00:00:00.000Z")
        .await
        .unwrap();

    let health = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "board_health",
        serde_json::json!({ "project": project.slug() }),
    )
    .await;

    assert!(
        health["stale_tasks"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
            >= 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_contract_board_reconcile_requires_pool() {
    let app = test_helpers::create_test_app();
    let session_id = test_helpers::initialize_mcp_session(&app).await;

    let _ = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "project_add",
        serde_json::json!({
            "name": "contract-board-reconcile-empty",
            "path": CONTRACT_PROJECT_PATH,
        }),
    )
    .await;

    let result = test_helpers::mcp_call_tool(
        &app,
        &session_id,
        "board_reconcile",
        serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
    )
    .await;

    // board_reconcile requires the slot pool actor, which is not started in tests
    assert!(result.get("error").and_then(|v| v.as_str()).is_some());
}
