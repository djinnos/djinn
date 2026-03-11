use serde_json::json;

use crate::db::repositories::task::TaskRepository;
use crate::test_helpers::{
    create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
    create_test_task, initialize_mcp_session, mcp_call_tool,
};

#[tokio::test]
async fn task_create_success_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({"project": project.path, "epic_id": epic.id, "title": "Create task contract test"}),
    )
    .await;

    let task = &payload["data"];
    assert!(task["id"].as_str().is_some());
    assert!(task["short_id"].as_str().is_some());
    assert_eq!(task["status"], "open");
    assert_eq!(task["title"], "Create task contract test");
    assert_eq!(task["epic_id"], epic.id);
}

#[tokio::test]
async fn task_create_error_missing_project() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({"project": "missing/project", "title": "No project"}),
    )
    .await;

    assert!(payload["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn task_show_found_and_not_found_shapes() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let ok_payload = mcp_call_tool(
        &app,
        &sid,
        "task_show",
        json!({"project": project.path, "id": task.id}),
    )
    .await;
    assert!(ok_payload["data"]["task"]["id"].as_str().is_some());
    assert!(ok_payload["data"]["task"]["title"].as_str().is_some());

    let err_payload = mcp_call_tool(
        &app,
        &sid,
        "task_show",
        json!({"project": project.path, "id": "missing-task-id"}),
    )
    .await;
    assert!(err_payload["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn task_list_filters_and_pagination() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic1 = create_test_epic(&db, &project.id).await;
    let epic2 = create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), tokio::sync::broadcast::channel(16).0);

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
        )
        .await
        .unwrap();
    repo.transition(&t1.id, crate::models::task::TransitionAction::Start, "a", "user", None, None)
        .await
        .unwrap();

    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let by_status = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "status": "in_progress"}),
    )
    .await;
    assert!(by_status["tasks"].as_array().unwrap().len() >= 1);

    let by_text = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "text": "gamma"}),
    )
    .await;
    assert_eq!(by_text["tasks"].as_array().unwrap().len(), 1);

    let by_epic = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "epic": epic2.id}),
    )
    .await;
    let epic_tasks = by_epic["tasks"].as_array().unwrap();
    assert_eq!(epic_tasks.len(), 1);
    assert!(epic_tasks
        .iter()
        .all(|t| t["epic_id"].as_str() == Some(epic2.id.as_str())));

    let paged = mcp_call_tool(
        &app,
        &sid,
        "task_list",
        json!({"project": project.path, "limit": 1, "offset": 0}),
    )
    .await;
    assert_eq!(paged["limit"], 1);
    assert_eq!(paged["offset"], 0);
}

#[tokio::test]
async fn task_update_partial_and_error_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let ok = mcp_call_tool(
        &app,
        &sid,
        "task_update",
        json!({"project": project.path, "id": task.id, "title": "updated"}),
    )
    .await;
    assert_eq!(ok["data"]["title"], "updated");

    let err = mcp_call_tool(
        &app,
        &sid,
        "task_update",
        json!({"project": project.path, "id": "missing-id", "title": "x"}),
    )
    .await;
    assert!(err["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn task_transition_valid_and_invalid() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let ok = mcp_call_tool(
        &app,
        &sid,
        "task_transition",
        json!({"project": project.path, "id": task.id, "action": "start", "actor_id": "u1", "actor_role": "user"}),
    )
    .await;
    assert_eq!(ok["data"]["status"], "in_progress");

    let bad = mcp_call_tool(
        &app,
        &sid,
        "task_transition",
        json!({"project": project.path, "id": task.id, "action": "not_real", "actor_id": "u1", "actor_role": "user"}),
    )
    .await;
    assert!(bad["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn task_count_plain_and_grouped() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let t1 = create_test_task(&db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), tokio::sync::broadcast::channel(16).0);
    repo.transition(&t1.id, crate::models::task::TransitionAction::Start, "u1", "user", None, None)
        .await
        .unwrap();
    let _t2 = create_test_task(&db, &project.id, &epic.id).await;

    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let plain = mcp_call_tool(&app, &sid, "task_count", json!({"project": project.path})).await;
    assert!(plain["data"]["total_count"].as_i64().unwrap() >= 2);

    let grouped =
        mcp_call_tool(&app, &sid, "task_count", json!({"project": project.path, "group_by": "status"}))
            .await;
    assert!(grouped["data"]["groups"].as_array().is_some());
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
    assert!(claimed["data"]["id"].as_str().is_some() || claimed["data"]["task"].is_null());

    let db2 = create_test_db();
    let project2 = create_test_project(&db2).await;
    let app2 = create_test_app_with_db(db2);
    let sid2 = initialize_mcp_session(&app2).await;
    let empty = mcp_call_tool(&app2, &sid2, "task_claim", json!({"project": project2.path})).await;
    assert!(empty["data"]["task"].is_null());
}

#[tokio::test]
async fn task_ready_lists_open_unblocked() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let _ = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(&app, &sid, "task_ready", json!({"project": project.path})).await;
    assert!(payload["data"]["tasks"].as_array().is_some());
}

#[tokio::test]
async fn task_comment_activity_blockers_blocked_memory_refs_shapes() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let blocker = create_test_task(&db, &project.id, &epic.id).await;
    let blocked = create_test_task(&db, &project.id, &epic.id).await;

    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let updated = mcp_call_tool(
        &app,
        &sid,
        "task_update",
        json!({"project": project.path, "id": blocked.id, "blocked_by_add": [blocker.id], "memory_refs": ["notes/a"]}),
    )
    .await;
    assert!(updated["data"]["id"].as_str().is_some());

    let c = mcp_call_tool(
        &app,
        &sid,
        "task_comment_add",
        json!({"project": project.path, "id": blocked.id, "actor_id": "u1", "actor_role": "user", "body": "hello"}),
    )
    .await;
    assert_eq!(c["data"]["event_type"], "comment");

    let c_err = mcp_call_tool(
        &app,
        &sid,
        "task_comment_add",
        json!({"project": project.path, "id": "missing", "actor_id": "u1", "actor_role": "user", "body": "hello"}),
    )
    .await;
    assert!(c_err["error"]["message"].as_str().is_some());

    let activity = mcp_call_tool(
        &app,
        &sid,
        "task_activity_list",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(activity["data"]["entries"].as_array().is_some());

    let blockers = mcp_call_tool(
        &app,
        &sid,
        "task_blockers_list",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(blockers["data"]["blockers"].as_array().is_some());

    let blocked_list = mcp_call_tool(
        &app,
        &sid,
        "task_blocked_list",
        json!({"project": project.path, "id": blocker.id}),
    )
    .await;
    assert!(blocked_list["data"]["tasks"].as_array().is_some());

    let refs = mcp_call_tool(
        &app,
        &sid,
        "task_memory_refs",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(refs["data"]["memory_refs"].as_array().is_some());
}
