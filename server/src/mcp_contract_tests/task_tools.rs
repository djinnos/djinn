use insta::assert_json_snapshot;
use serde_json::json;

use crate::events::EventBus;
use crate::test_helpers::{
    create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
    create_test_task, initialize_mcp_session, mcp_call_tool,
};
use djinn_db::TaskRepository;

#[tokio::test]
async fn task_create_success_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(&app, &sid, "task_create", json!({"project": project.path, "epic_id": epic.id, "title": "Create task contract test", "acceptance_criteria": ["task is created successfully"]})).await;
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
    let db = create_test_db();
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({"project": "any/project", "title": ""}),
    )
    .await;
    assert!(payload["error"].as_str().is_some());
}

#[tokio::test]
async fn task_create_with_blocked_by_sets_blockers() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    // Create a blocker task first.
    let blocker = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({"project": project.path, "epic_id": epic.id, "title": "Blocker task", "acceptance_criteria": ["blocker is resolved"]}),
    )
    .await;
    let blocker_id = blocker["id"].as_str().unwrap();

    // Create a task that is blocked by the first.
    let blocked = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({
            "project": project.path,
            "epic_id": epic.id,
            "title": "Blocked task",
            "acceptance_criteria": ["blocked task completes after blocker"],
            "blocked_by": [blocker_id]
        }),
    )
    .await;
    assert!(blocked["id"].as_str().is_some(), "task should be created");
    assert!(blocked["error"].is_null(), "should not have error");

    // Verify blockers were persisted.
    let blockers = mcp_call_tool(
        &app,
        &sid,
        "task_blockers_list",
        json!({"project": project.path, "id": blocked["id"].as_str().unwrap()}),
    )
    .await;
    let blockers_arr = blockers["blockers"].as_array().unwrap();
    assert_eq!(blockers_arr.len(), 1);
    assert_eq!(blockers_arr[0]["blocking_task_id"], blocker_id);
}

#[tokio::test]
async fn task_create_with_invalid_blocked_by_rejects_without_creating_task() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let fake_id = "00000000-0000-0000-0000-000000000000";

    // Attempt to create a task with a non-existent blocker.
    let result = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({
            "project": project.path,
            "epic_id": epic.id,
            "title": "Should not exist",
            "acceptance_criteria": ["task completes"],
            "blocked_by": [fake_id]
        }),
    )
    .await;
    assert!(
        result["error"].as_str().is_some(),
        "should return error for invalid blocker"
    );

    // Verify no task was created (the task should not be in the DB).
    let list = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "text": "Should not exist"}),
    )
    .await;
    assert_eq!(
        list["total_count"].as_i64().unwrap(),
        0,
        "no task should have been created when blocked_by resolution fails"
    );
}

#[tokio::test]
async fn task_create_requires_acceptance_criteria_for_task_type() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    // task type without AC → error
    let result = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({"project": project.path, "epic_id": epic.id, "title": "No AC task", "issue_type": "task"}),
    )
    .await;
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|e| e.contains("acceptance_criteria")),
        "should error when acceptance_criteria is missing for task type, got: {result}"
    );

    // feature and bug types also require AC
    for issue_type in ["feature", "bug"] {
        let result = mcp_call_tool(
            &app,
            &sid,
            "task_create",
            json!({"project": project.path, "epic_id": epic.id, "title": "No AC", "issue_type": issue_type}),
        )
        .await;
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|e| e.contains("acceptance_criteria")),
            "{issue_type} should error without acceptance_criteria"
        );
    }
}

#[tokio::test]
async fn task_create_simple_lifecycle_types_do_not_require_acceptance_criteria() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    // Simple-lifecycle types should succeed without AC
    for issue_type in ["spike", "research", "planning", "review"] {
        let result = mcp_call_tool(
            &app,
            &sid,
            "task_create",
            json!({"project": project.path, "epic_id": epic.id, "title": format!("No AC {issue_type}"), "issue_type": issue_type}),
        )
        .await;
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
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let ok = mcp_call_tool(
        &app,
        &sid,
        "task_show",
        json!({"project": project.path, "id": task.id}),
    )
    .await;
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

    let err = mcp_call_tool(
        &app,
        &sid,
        "task_show",
        json!({"project": project.path, "id": "missing-task-id"}),
    )
    .await;
    assert!(err["error"].as_str().is_some());
}

#[tokio::test]
async fn task_list_filters_and_pagination() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic1 = create_test_epic(&db, &project.id).await;
    let epic2 = create_test_epic(&db, &project.id).await;
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

    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let by_status = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "status": "in_progress"}),
    )
    .await;
    assert!(!by_status["tasks"].as_array().unwrap().is_empty());

    let by_text = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "text": "gamma"}),
    )
    .await;
    assert_eq!(by_text["tasks"].as_array().unwrap().len(), 1);

    let paged = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "limit": 1, "offset": 0}),
    )
    .await;
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
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let ok = mcp_call_tool(
        &app,
        &sid,
        "task_update",
        json!({"project": project.path, "id": task.id, "title": "updated"}),
    )
    .await;
    assert_eq!(ok["title"], "updated");

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    assert_eq!(repo.get(&task.id).await.unwrap().unwrap().title, "updated");

    let err = mcp_call_tool(
        &app,
        &sid,
        "task_update",
        json!({"project": project.path, "id": "missing-id", "title": "x"}),
    )
    .await;
    assert!(err["error"].as_str().is_some());
}

#[tokio::test]
async fn task_transition_valid_and_invalid() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let ok = mcp_call_tool(&app, &sid, "task_transition", json!({"project": project.path, "id": task.id, "action": "start", "actor_id": "u1", "actor_role": "user"})).await;
    assert_eq!(ok["status"], "in_progress");

    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    assert_eq!(
        repo.get(&task.id).await.unwrap().unwrap().status,
        "in_progress"
    );

    let bad = mcp_call_tool(&app, &sid, "task_transition", json!({"project": project.path, "id": task.id, "action": "not_real", "actor_id": "u1", "actor_role": "user"})).await;
    assert!(bad["error"].as_str().is_some());
}

#[tokio::test]
async fn task_count_plain_and_grouped() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let t1 = create_test_task(&db, &project.id, &epic.id).await;
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
    let _t2 = create_test_task(&db, &project.id, &epic.id).await;

    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let plain = mcp_call_tool(&app, &sid, "task_count", json!({"project": project.path})).await;
    assert!(plain["total_count"].as_i64().unwrap() >= 2);
    assert_json_snapshot!("task_count_plain_response", plain);

    let grouped = mcp_call_tool(
        &app,
        &sid,
        "task_count",
        json!({"project": project.path, "group_by": "status"}),
    )
    .await;
    assert!(grouped["groups"].as_array().is_some());
    assert_json_snapshot!("task_count_grouped_by_status_response", grouped);

    let priority_grouped = mcp_call_tool(
        &app,
        &sid,
        "task_count",
        json!({"project": project.path, "group_by": "priority"}),
    )
    .await;
    assert!(priority_grouped["groups"].as_array().is_some());
    assert_json_snapshot!("task_count_grouped_by_priority_response", priority_grouped);
}

#[tokio::test]
async fn task_claim_ready_and_empty() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let _task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let claimed = mcp_call_tool(&app, &sid, "task_claim", json!({"project": project.path})).await;
    assert!(claimed["id"].as_str().is_some() || claimed["task"].is_null());

    let db2 = create_test_db();
    let project2 = create_test_project(&db2).await;
    let app2 = create_test_app_with_db(db2);
    let sid2 = initialize_mcp_session(&app2).await;
    let empty = mcp_call_tool(
        &app2,
        &sid2,
        "task_claim",
        json!({"project": project2.path}),
    )
    .await;
    assert!(empty["task"].is_null());
}

#[tokio::test]
async fn task_ready_lists_open_unblocked() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let _ = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(&app, &sid, "task_ready", json!({"project": project.path})).await;
    assert!(payload["tasks"].as_array().is_some());
}

#[tokio::test]
async fn task_comment_activity_blockers_blocked_memory_refs_shapes() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let blocker = create_test_task(&db, &project.id, &epic.id).await;
    let blocked = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let updated = mcp_call_tool(&app, &sid, "task_update", json!({"project": project.path, "id": blocked.id, "blocked_by_add": [blocker.id], "memory_refs_add": ["notes/a"]})).await;
    assert!(updated["id"].as_str().is_some());

    let c = mcp_call_tool(&app, &sid, "task_comment_add", json!({"project": project.path, "id": blocked.id, "actor_id": "u1", "actor_role": "user", "body": "hello"})).await;
    assert_eq!(c["event_type"], "comment");

    let c_err = mcp_call_tool(&app, &sid, "task_comment_add", json!({"project": project.path, "id": "missing", "actor_id": "u1", "actor_role": "user", "body": "hello"})).await;
    assert!(c_err["error"].as_str().is_some());

    let activity = mcp_call_tool(
        &app,
        &sid,
        "task_activity_list",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(activity["entries"].as_array().is_some());
    assert_json_snapshot!("task_activity_list_response", activity, {
        ".entries.**.id" => "[UUID]",
        ".entries.**.task_id" => "[UUID]",
        ".entries.**.actor_id" => "[UUID]",
        ".entries.**.created_at" => "[TIMESTAMP]",
        ".entries.**.timestamp" => "[TIMESTAMP]"
    });

    let blockers = mcp_call_tool(
        &app,
        &sid,
        "task_blockers_list",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(blockers["blockers"].as_array().is_some());
    assert_json_snapshot!("task_blockers_list_response", blockers, {
        ".blockers.**.blocking_task_id" => "[UUID]",
        ".blockers.**.blocking_task_short_id" => "[SHORT_ID]"
    });

    let blocked_list = mcp_call_tool(
        &app,
        &sid,
        "task_blocked_list",
        json!({"project": project.path, "id": blocker.id}),
    )
    .await;
    assert!(blocked_list["tasks"].as_array().is_some());

    let refs = mcp_call_tool(
        &app,
        &sid,
        "task_memory_refs",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(refs["memory_refs"].as_array().is_some());
}
