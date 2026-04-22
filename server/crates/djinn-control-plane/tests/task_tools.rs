//! Contract tests for `task_*` MCP tools.
//!
//! Migrated verbatim from `server/src/mcp_contract_tests/task_tools.rs`.
//! Snapshot files (`insta` `.snap`) live under `tests/snapshots/` with the
//! `task_tools__` prefix insta generates for integration tests.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_db::TaskRepository;
use insta::assert_json_snapshot;
use serde_json::json;

#[tokio::test]
async fn task_create_success_shape() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;

    let payload = harness
        .call_tool(
            "task_create",
            json!({"project": project.path, "epic_id": epic.id, "title": "Create task contract test", "acceptance_criteria": ["task is created successfully"]}),
        )
        .await
        .expect("task_create should dispatch");
    assert!(payload["id"].as_str().is_some());
    assert!(payload["short_id"].as_str().is_some());
    assert_eq!(payload["status"], "open");
    assert_eq!(payload["title"], "Create task contract test");
    assert_eq!(payload["epic_id"], epic.id);

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let created = repo
        .get(payload["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(created.title, "Create task contract test");
    assert_eq!(created.status, "open");
    assert_eq!(created.epic_id, Some(epic.id));
}

#[tokio::test]
async fn task_create_error_validation() {
    let harness = McpTestHarness::new().await;

    // `task_create` validation failures surface through `ErrorOr::Error` which
    // the dispatcher converts into `Err(String)`; that's the new contract.
    let err = harness
        .call_tool(
            "task_create",
            json!({"project": "any/project", "title": ""}),
        )
        .await
        .expect_err("task_create with empty title should error");
    let msg = err.to_string();
    assert!(
        msg.contains("task_create") || msg.contains("title"),
        "error should identify task_create or title, got: {msg}"
    );
}

#[tokio::test]
async fn task_create_with_blocked_by_sets_blockers() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;

    // Create a blocker task first.
    let blocker = harness
        .call_tool(
            "task_create",
            json!({"project": project.path, "epic_id": epic.id, "title": "Blocker task", "acceptance_criteria": ["blocker is resolved"]}),
        )
        .await
        .expect("blocker task_create should dispatch");
    let blocker_id = blocker["id"].as_str().unwrap();

    // Create a task that is blocked by the first.
    let blocked = harness
        .call_tool(
            "task_create",
            json!({
                "project": project.path,
                "epic_id": epic.id,
                "title": "Blocked task",
                "acceptance_criteria": ["blocked task completes after blocker"],
                "blocked_by": [blocker_id]
            }),
        )
        .await
        .expect("blocked task_create should dispatch");
    assert!(blocked["id"].as_str().is_some(), "task should be created");
    assert!(blocked["error"].is_null(), "should not have error");

    // Verify blockers were persisted.
    let blockers = harness
        .call_tool(
            "task_blockers_list",
            json!({"project": project.path, "id": blocked["id"].as_str().unwrap()}),
        )
        .await
        .expect("task_blockers_list should dispatch");
    let blockers_arr = blockers["blockers"].as_array().unwrap();
    assert_eq!(blockers_arr.len(), 1);
    assert_eq!(blockers_arr[0]["blocking_task_id"], blocker_id);
}

#[tokio::test]
async fn task_create_with_invalid_blocked_by_rejects_without_creating_task() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;

    let fake_id = "00000000-0000-0000-0000-000000000000";

    // Attempt to create a task with a non-existent blocker.
    let err = harness
        .call_tool(
            "task_create",
            json!({
                "project": project.path,
                "epic_id": epic.id,
                "title": "Should not exist",
                "acceptance_criteria": ["task completes"],
                "blocked_by": [fake_id]
            }),
        )
        .await
        .expect_err("task_create with invalid blocker should error");
    let msg = err.to_string();
    assert!(
        msg.contains("task_create"),
        "error should identify task_create, got: {msg}"
    );

    // Verify no task was created (the task should not be in the DB).
    let list = harness
        .call_tool(
            "task_list",
            json!({"project": project.path, "text": "Should not exist"}),
        )
        .await
        .expect("task_list should dispatch");
    assert_eq!(
        list["total_count"].as_i64().unwrap(),
        0,
        "no task should have been created when blocked_by resolution fails"
    );
}

#[tokio::test]
async fn task_create_requires_acceptance_criteria_for_task_type() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;

    // task type without AC → error
    let err = harness
        .call_tool(
            "task_create",
            json!({"project": project.path, "epic_id": epic.id, "title": "No AC task", "issue_type": "task"}),
        )
        .await
        .expect_err("task type without AC should error");
    assert!(
        err.to_string().contains("acceptance_criteria"),
        "should error with 'acceptance_criteria' hint, got: {err}"
    );

    // feature and bug types also require AC
    for issue_type in ["feature", "bug"] {
        let err = harness
            .call_tool(
                "task_create",
                json!({"project": project.path, "epic_id": epic.id, "title": "No AC", "issue_type": issue_type}),
            )
            .await
            .expect_err("{issue_type} should error without AC");
        assert!(
            err.to_string().contains("acceptance_criteria"),
            "{issue_type} should error without acceptance_criteria, got: {err}"
        );
    }
}

#[tokio::test]
async fn task_create_simple_lifecycle_types_do_not_require_acceptance_criteria() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;

    // Simple-lifecycle types should succeed without AC
    for issue_type in ["spike", "research", "planning", "review"] {
        let result = harness
            .call_tool(
                "task_create",
                json!({"project": project.path, "epic_id": epic.id, "title": format!("No AC {issue_type}"), "issue_type": issue_type}),
            )
            .await
            .expect("{issue_type} task_create should dispatch");
        assert!(
            result.get("error").is_none() || result["error"].is_null(),
            "{issue_type} should not require acceptance_criteria, got: {result}"
        );
        assert!(
            result["id"].as_str().is_some(),
            "{issue_type} task should be created"
        );
    }
}

#[tokio::test]
async fn task_show_found_and_not_found_shapes() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;

    let ok = harness
        .call_tool(
            "task_show",
            json!({"project": project.path, "id": task.id}),
        )
        .await
        .expect("task_show should dispatch");
    assert!(ok["id"].as_str().is_some());
    assert!(ok["title"].as_str().is_some());
    assert_json_snapshot!("task_show_response", ok, {
        ".id" => "[UUID]",
        ".epic_id" => "[UUID]",
        ".project_id" => "[UUID]",
        ".short_id" => "[SHORT_ID]",
        ".created_at" => "[TIMESTAMP]",
        ".updated_at" => "[TIMESTAMP]"
    });

    let err = harness
        .call_tool(
            "task_show",
            json!({"project": project.path, "id": "missing-task-id"}),
        )
        .await
        .expect_err("task_show with missing id should error");
    assert!(
        err.to_string().contains("task_show"),
        "error should identify task_show, got: {err}"
    );
}

#[tokio::test]
async fn task_list_filters_and_pagination() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic1 = common::create_test_epic(db, &project.id).await;
    let epic2 = common::create_test_epic(db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), EventBus::noop());

    let t1 = repo
        .create_in_project(
            &project.id,
            Some(&epic1.id),
            "alpha ready",
            "desc",
            "design",
            "task",
            1,
            "owner",
            None,
            None,
        )
        .await
        .unwrap();
    let _t2 = repo
        .create_in_project(
            &project.id,
            Some(&epic1.id),
            "beta progress",
            "desc",
            "design",
            "task",
            2,
            "owner",
            None,
            None,
        )
        .await
        .unwrap();
    let _t3 = repo
        .create_in_project(
            &project.id,
            Some(&epic2.id),
            "gamma text",
            "desc",
            "design",
            "task",
            3,
            "owner",
            None,
            None,
        )
        .await
        .unwrap();
    repo.update(
        &t1.id,
        "alpha ready",
        "desc",
        "design",
        1,
        "owner",
        "",
        r#"[{"description":"default","met":false}]"#,
    )
    .await
    .unwrap();
    repo.transition(
        &t1.id,
        djinn_core::models::TransitionAction::Start,
        "a",
        "user",
        None,
        None,
    )
    .await
    .unwrap();

    let by_status = harness
        .call_tool(
            "task_list",
            json!({"project": project.path, "status": "in_progress"}),
        )
        .await
        .expect("task_list by_status should dispatch");
    assert!(!by_status["tasks"].as_array().unwrap().is_empty());

    let by_text = harness
        .call_tool(
            "task_list",
            json!({"project": project.path, "text": "gamma"}),
        )
        .await
        .expect("task_list by_text should dispatch");
    assert_eq!(by_text["tasks"].as_array().unwrap().len(), 1);

    let paged = harness
        .call_tool(
            "task_list",
            json!({"project": project.path, "limit": 1, "offset": 0}),
        )
        .await
        .expect("task_list paged should dispatch");
    assert_eq!(paged["limit"], 1);
    assert_eq!(paged["offset"], 0);
    assert_json_snapshot!("task_list_response", paged, {
        ".tasks.**.id" => "[UUID]",
        ".tasks.**.epic_id" => "[UUID]",
        ".tasks.**.project_id" => "[UUID]",
        ".tasks.**.short_id" => "[SHORT_ID]",
        ".tasks.**.created_at" => "[TIMESTAMP]",
        ".tasks.**.updated_at" => "[TIMESTAMP]"
    });
}

#[tokio::test]
async fn task_update_partial_and_error_shape() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;

    let ok = harness
        .call_tool(
            "task_update",
            json!({"project": project.path, "id": task.id, "title": "updated"}),
        )
        .await
        .expect("task_update should dispatch");
    assert_eq!(ok["title"], "updated");

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    assert_eq!(repo.get(&task.id).await.unwrap().unwrap().title, "updated");

    let err = harness
        .call_tool(
            "task_update",
            json!({"project": project.path, "id": "missing-id", "title": "x"}),
        )
        .await
        .expect_err("task_update with missing id should error");
    assert!(
        err.to_string().contains("task_update"),
        "error should identify task_update, got: {err}"
    );
}

#[tokio::test]
async fn task_transition_valid_and_invalid() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;

    let ok = harness
        .call_tool(
            "task_transition",
            json!({"project": project.path, "id": task.id, "action": "start", "actor_id": "u1", "actor_role": "user"}),
        )
        .await
        .expect("task_transition start should dispatch");
    assert_eq!(ok["status"], "in_progress");

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    assert_eq!(
        repo.get(&task.id).await.unwrap().unwrap().status,
        "in_progress"
    );

    let err = harness
        .call_tool(
            "task_transition",
            json!({"project": project.path, "id": task.id, "action": "not_real", "actor_id": "u1", "actor_role": "user"}),
        )
        .await
        .expect_err("invalid transition action should error");
    assert!(
        err.to_string().contains("task_transition"),
        "error should identify task_transition, got: {err}"
    );
}

#[tokio::test]
async fn task_count_plain_and_grouped() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let t1 = common::create_test_task(db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    repo.transition(
        &t1.id,
        djinn_core::models::TransitionAction::Start,
        "u1",
        "user",
        None,
        None,
    )
    .await
    .unwrap();
    let _t2 = common::create_test_task(db, &project.id, &epic.id).await;

    let plain = harness
        .call_tool("task_count", json!({"project": project.path}))
        .await
        .expect("task_count plain should dispatch");
    assert!(plain["total_count"].as_i64().unwrap() >= 2);
    assert_json_snapshot!("task_count_plain_response", plain);

    let grouped = harness
        .call_tool(
            "task_count",
            json!({"project": project.path, "group_by": "status"}),
        )
        .await
        .expect("task_count grouped should dispatch");
    assert!(grouped["groups"].as_array().is_some());
    assert_json_snapshot!("task_count_grouped_by_status_response", grouped);

    let priority_grouped = harness
        .call_tool(
            "task_count",
            json!({"project": project.path, "group_by": "priority"}),
        )
        .await
        .expect("task_count priority grouped should dispatch");
    assert!(priority_grouped["groups"].as_array().is_some());
    assert_json_snapshot!("task_count_grouped_by_priority_response", priority_grouped);
}

#[tokio::test]
async fn task_claim_ready_and_empty() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let _task = common::create_test_task(db, &project.id, &epic.id).await;

    let claimed = harness
        .call_tool("task_claim", json!({"project": project.path}))
        .await
        .expect("task_claim should dispatch");
    assert!(claimed["id"].as_str().is_some() || claimed["task"].is_null());

    let harness2 = McpTestHarness::new().await;
    let project2 = common::create_test_project(harness2.db()).await;
    let empty = harness2
        .call_tool("task_claim", json!({"project": project2.path}))
        .await
        .expect("task_claim on empty project should dispatch");
    assert!(empty["task"].is_null());
}

#[tokio::test]
async fn task_ready_lists_open_unblocked() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let _ = common::create_test_task(db, &project.id, &epic.id).await;

    let payload = harness
        .call_tool("task_ready", json!({"project": project.path}))
        .await
        .expect("task_ready should dispatch");
    assert!(payload["tasks"].as_array().is_some());
}

#[tokio::test]
async fn task_comment_activity_blockers_blocked_memory_refs_shapes() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let blocker = common::create_test_task(db, &project.id, &epic.id).await;
    let blocked = common::create_test_task(db, &project.id, &epic.id).await;

    let updated = harness
        .call_tool(
            "task_update",
            json!({"project": project.path, "id": blocked.id, "blocked_by_add": [blocker.id], "memory_refs_add": ["notes/a"]}),
        )
        .await
        .expect("task_update should dispatch");
    assert!(updated["id"].as_str().is_some());

    let c = harness
        .call_tool(
            "task_comment_add",
            json!({"project": project.path, "id": blocked.id, "actor_id": "u1", "actor_role": "user", "body": "hello"}),
        )
        .await
        .expect("task_comment_add should dispatch");
    assert_eq!(c["event_type"], "comment");

    let c_err = harness
        .call_tool(
            "task_comment_add",
            json!({"project": project.path, "id": "missing", "actor_id": "u1", "actor_role": "user", "body": "hello"}),
        )
        .await
        .expect_err("task_comment_add with missing task should error");
    assert!(
        c_err.to_string().contains("task_comment_add"),
        "error should identify task_comment_add, got: {c_err}"
    );

    let activity = harness
        .call_tool(
            "task_activity_list",
            json!({"project": project.path, "id": blocked.id}),
        )
        .await
        .expect("task_activity_list should dispatch");
    assert!(activity["entries"].as_array().is_some());
    assert_json_snapshot!("task_activity_list_response", activity, {
        ".entries.**.id" => "[UUID]",
        ".entries.**.task_id" => "[UUID]",
        ".entries.**.actor_id" => "[UUID]",
        ".entries.**.created_at" => "[TIMESTAMP]",
        ".entries.**.timestamp" => "[TIMESTAMP]"
    });

    let blockers = harness
        .call_tool(
            "task_blockers_list",
            json!({"project": project.path, "id": blocked.id}),
        )
        .await
        .expect("task_blockers_list should dispatch");
    assert!(blockers["blockers"].as_array().is_some());
    assert_json_snapshot!("task_blockers_list_response", blockers, {
        ".blockers.**.blocking_task_id" => "[UUID]",
        ".blockers.**.blocking_task_short_id" => "[SHORT_ID]"
    });

    let blocked_list = harness
        .call_tool(
            "task_blocked_list",
            json!({"project": project.path, "id": blocker.id}),
        )
        .await
        .expect("task_blocked_list should dispatch");
    assert!(blocked_list["tasks"].as_array().is_some());

    let refs = harness
        .call_tool(
            "task_memory_refs",
            json!({"project": project.path, "id": blocked.id}),
        )
        .await
        .expect("task_memory_refs should dispatch");
    assert!(refs["memory_refs"].as_array().is_some());
}
