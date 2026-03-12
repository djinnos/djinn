use serde_json::json;

use crate::db::repositories::session_message::SessionMessageRepository;
use crate::test_helpers::{
    create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
    create_test_session, create_test_task, initialize_mcp_session, mcp_call_tool,
};

#[tokio::test]
async fn session_list_returns_empty_for_task_without_sessions() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &session_id,
        "session_list",
        json!({ "task_id": task.id, "project": project.path }),
    )
    .await;

    assert_eq!(payload.get("error"), None);
    assert_eq!(payload.get("task_id").and_then(|v| v.as_str()), Some(task.id.as_str()));
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    assert!(sessions.is_empty());
}

#[tokio::test]
async fn session_list_filters_by_project_and_task() {
    let db = create_test_db();
    let project_a = create_test_project(&db).await;
    let epic_a = create_test_epic(&db, &project_a.id).await;
    let task_a1 = create_test_task(&db, &project_a.id, &epic_a.id).await;
    let task_a2 = create_test_task(&db, &project_a.id, &epic_a.id).await;

    let project_b = create_test_project(&db).await;
    let epic_b = create_test_epic(&db, &project_b.id).await;
    let task_b1 = create_test_task(&db, &project_b.id, &epic_b.id).await;

    let _s_a1_1 = create_test_session(&db, &project_a.id, &task_a1.id).await;
    let _s_a1_2 = create_test_session(&db, &project_a.id, &task_a1.id).await;
    let _s_a2 = create_test_session(&db, &project_a.id, &task_a2.id).await;
    let _s_b1 = create_test_session(&db, &project_b.id, &task_b1.id).await;

    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &session_id,
        "session_list",
        json!({ "task_id": task_a1.id, "project": project_a.path }),
    )
    .await;

    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions
        .iter()
        .all(|s| s.get("task_id").and_then(|v| v.as_str()) == Some(task_a1.id.as_str())));
    assert!(sessions
        .iter()
        .all(|s| s.get("project_id").and_then(|v| v.as_str()) == Some(project_a.id.as_str())));
}

#[tokio::test]
async fn session_show_returns_full_shape_with_tokens() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let session = create_test_session(&db, &project.id, &task.id).await;

    let app = create_test_app_with_db(db);
    let mcp_session = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &mcp_session,
        "session_show",
        json!({ "id": session.id, "project": project.path }),
    )
    .await;

    assert_eq!(payload.get("error"), None);
    for key in [
        "id",
        "task_id",
        "model_id",
        "agent_type",
        "status",
        "tokens_in",
        "tokens_out",
    ] {
        assert!(payload.get(key).is_some(), "missing key {key}");
    }
}

#[tokio::test]
async fn session_show_not_found_returns_error_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &session_id,
        "session_show",
        json!({ "id": "missing-session-id", "project": project.path }),
    )
    .await;

    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert_eq!(payload.get("id"), None);
}

#[tokio::test]
async fn session_active_returns_error_without_pool() {
    // session_active requires the slot pool actor, which is not started in unit tests.
    // Verify it returns a graceful error rather than panicking.
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &session_id,
        "session_active",
        json!({ "project": project.path }),
    )
    .await;

    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn session_for_task_returns_error_without_pool() {
    // session_for_task requires the slot pool actor (not just DB lookup).
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let _session = create_test_session(&db, &project.id, &task.id).await;
    let app = create_test_app_with_db(db);
    let mcp_session = initialize_mcp_session(&app).await;

    let result = mcp_call_tool(
        &app,
        &mcp_session,
        "session_for_task",
        json!({ "task_id": task.id, "project": project.path }),
    )
    .await;
    assert!(result.get("error").and_then(|v| v.as_str()).is_some());
}

#[tokio::test]
async fn task_timeline_returns_chronological_session_and_message_history() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let s1 = create_test_session(&db, &project.id, &task.id).await;
    let s2 = create_test_session(&db, &project.id, &task.id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), tokio::sync::broadcast::channel(16).0);
    msg_repo
        .insert_message(
            &s1.id,
            &task.id,
            "user",
            &json!([{"type":"text","text":"first"}]).to_string(),
            None,
        )
        .await
        .unwrap();
    msg_repo
        .insert_message(
            &s2.id,
            &task.id,
            "assistant",
            &json!([{"type":"text","text":"second"}]).to_string(),
            None,
        )
        .await
        .unwrap();

    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &session_id,
        "task_timeline",
        json!({ "task_id": task.id, "project": project.path }),
    )
    .await;

    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(messages.len(), 2);
    let ts0 = messages[0].get("timestamp").and_then(|v| v.as_str()).unwrap();
    let ts1 = messages[1].get("timestamp").and_then(|v| v.as_str()).unwrap();
    assert!(ts0 <= ts1);
}

#[tokio::test]
async fn task_timeline_not_found_returns_error_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let payload = mcp_call_tool(
        &app,
        &session_id,
        "task_timeline",
        json!({ "task_id": "missing-task", "project": project.path }),
    )
    .await;

    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert!(payload.get("sessions").is_none());
    assert!(payload.get("messages").is_none());
}


#[tokio::test]
async fn session_messages_returns_messages_for_valid_session_id() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let sess = create_test_session(&db, &project.id, &task.id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), tokio::sync::broadcast::channel(16).0);
    msg_repo
        .insert_message(
            &sess.id,
            &task.id,
            "user",
            &json!([{"type":"text","text":"hello"}]).to_string(),
            None,
        )
        .await
        .unwrap();

    let app = create_test_app_with_db(db);
    let mcp_session = initialize_mcp_session(&app).await;
    let payload = mcp_call_tool(
        &app,
        &mcp_session,
        "session_messages",
        json!({ "id": sess.id, "project": project.path }),
    )
    .await;

    assert_eq!(payload.get("error"), None);
    let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].get("role").and_then(|v| v.as_str()), Some("user"));
}
