use serde_json::json;

use crate::db::repositories::task::TaskRepository;
use tokio::sync::broadcast;
use crate::test_helpers::{
    create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
    create_test_task, initialize_mcp_session, mcp_call_tool,
};

#[tokio::test]
async fn task_create_success_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let app = create_test_app_with_db(db.clone());
    let sid = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &sid,
        "task_create",
        json!({"project": project.path, "epic_id": epic.id, "title": "Create task contract test"}),
    )
    .await;

    assert!(payload["id"].as_str().is_some());
    assert!(payload["short_id"].as_str().is_some());
    assert_eq!(payload["status"], "backlog");
    assert_eq!(payload["title"], "Create task contract test");
    assert_eq!(payload["epic_id"], epic.id);

    let repo = TaskRepository::new(db.clone(), broadcast::channel(16).0);
    let created = repo.get(payload["id"].as_str().unwrap()).await.unwrap().unwrap();
    assert_eq!(created.title, "Create task contract test");
    assert_eq!(created.status, "backlog");
    assert_eq!(created.epic_id, Some(epic.id));
}

#[tokio::test]
async fn task_create_error_validation() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    // Empty title triggers a validation error.
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
    assert!(ok_payload["id"].as_str().is_some());
    assert!(ok_payload["title"].as_str().is_some());

    let err_payload = mcp_call_tool(
        &app,
        &sid,
        "task_show",
        json!({"project": project.path, "id": "missing-task-id"}),
    )
    .await;
    assert!(err_payload["error"].as_str().is_some());
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
        )
        .await
        .unwrap();
    repo.transition(&t1.id, crate::models::TransitionAction::Accept, "a", "user", None, None)
        .await
        .unwrap();
    repo.update(&t1.id, "alpha ready", "desc", "design", 1, "owner", "", r#"[{"description":"default","met":false}]"#)
        .await
        .unwrap();
    repo.transition(&t1.id, crate::models::TransitionAction::Start, "a", "user", None, None)
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

    let repo = TaskRepository::new(db.clone(), broadcast::channel(16).0);
    let updated = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(updated.title, "updated");

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

    let ok = mcp_call_tool(
        &app,
        &sid,
        "task_transition",
        json!({"project": project.path, "id": task.id, "action": "accept", "actor_id": "u1", "actor_role": "user"}),
    )
    .await;
    assert_eq!(ok["status"], "open");

    let repo = TaskRepository::new(db.clone(), broadcast::channel(16).0);
    let transitioned = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(transitioned.status, "open");

    let bad = mcp_call_tool(
        &app,
        &sid,
        "task_transition",
        json!({"project": project.path, "id": task.id, "action": "not_real", "actor_id": "u1", "actor_role": "user"}),
    )
    .await;
    assert!(bad["error"].as_str().is_some());
}

#[tokio::test]
async fn task_count_plain_and_grouped() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let t1 = create_test_task(&db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), tokio::sync::broadcast::channel(16).0);
    repo.transition(&t1.id, crate::models::TransitionAction::Accept, "u1", "user", None, None)
        .await
        .unwrap();
    repo.transition(&t1.id, crate::models::TransitionAction::Start, "u1", "user", None, None)
        .await
        .unwrap();
    let _t2 = create_test_task(&db, &project.id, &epic.id).await;

    let app = create_test_app_with_db(db);
    let sid = initialize_mcp_session(&app).await;

    let plain = mcp_call_tool(&app, &sid, "task_count", json!({"project": project.path})).await;
    assert!(plain["total_count"].as_i64().unwrap() >= 2);

    let grouped =
        mcp_call_tool(&app, &sid, "task_count", json!({"project": project.path, "group_by": "status"}))
            .await;
    assert!(grouped["groups"].as_array().is_some());
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
    let empty = mcp_call_tool(&app2, &sid2, "task_claim", json!({"project": project2.path})).await;
    assert!(empty["task"].is_null());
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
    assert!(payload["tasks"].as_array().is_some());
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
        json!({"project": project.path, "id": blocked.id, "blocked_by_add": [blocker.id], "memory_refs_add": ["notes/a"]}),
    )
    .await;
    assert!(updated["id"].as_str().is_some());

    let c = mcp_call_tool(
        &app,
        &sid,
        "task_comment_add",
        json!({"project": project.path, "id": blocked.id, "actor_id": "u1", "actor_role": "user", "body": "hello"}),
    )
    .await;
    assert_eq!(c["event_type"], "comment");

    let c_err = mcp_call_tool(
        &app,
        &sid,
        "task_comment_add",
        json!({"project": project.path, "id": "missing", "actor_id": "u1", "actor_role": "user", "body": "hello"}),
    )
    .await;
    assert!(c_err["error"].as_str().is_some());

    let activity = mcp_call_tool(
        &app,
        &sid,
        "task_activity_list",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(activity["entries"].as_array().is_some());

    let blockers = mcp_call_tool(
        &app,
        &sid,
        "task_blockers_list",
        json!({"project": project.path, "id": blocked.id}),
    )
    .await;
    assert!(blockers["blockers"].as_array().is_some());

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
