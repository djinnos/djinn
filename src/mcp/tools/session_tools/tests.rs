use serde_json::json;

use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::session_message::SessionMessageRepository;
use crate::test_helpers::{
    create_test_app, create_test_db, create_test_epic, create_test_project, create_test_session,
    create_test_task, mcp_call_tool,
};

#[tokio::test]
async fn session_list_returns_empty_for_task_without_sessions() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let app = create_test_app();

    let payload = mcp_call_tool(
        &app,
        "test-session-list-empty",
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

    let app = create_test_app();
    let payload = mcp_call_tool(
        &app,
        "test-session-list-filter",
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

    let app = create_test_app();
    let payload = mcp_call_tool(
        &app,
        "test-session-show-found",
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
    let app = create_test_app();

    let payload = mcp_call_tool(
        &app,
        "test-session-show-missing",
        "session_show",
        json!({ "id": "missing-session-id", "project": project.path }),
    )
    .await;

    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert_eq!(payload.get("id"), None);
}

#[tokio::test]
async fn session_active_returns_empty_when_none_running() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let app = create_test_app();

    let payload = mcp_call_tool(
        &app,
        "test-session-active-empty",
        "session_active",
        json!({ "project": project.path }),
    )
    .await;

    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    assert!(sessions.is_empty());
}

#[tokio::test]
async fn session_active_includes_running_session() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let mut session = create_test_session(&db, &project.id, &task.id).await;

    let repo = SessionRepository::new(db.clone(), tokio::sync::broadcast::channel(16).0);
    session.status = "running".to_string();
    repo.update(&session).await.unwrap();

    let app = create_test_app();
    let payload = mcp_call_tool(
        &app,
        "test-session-active-running",
        "session_active",
        json!({ "project": project.path }),
    )
    .await;

    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    let stale = payload
        .get("stale_sessions")
        .and_then(|v| v.as_array())
        .unwrap();
    assert!(sessions.is_empty());
    assert!(stale.iter().any(|s| s.get("id").and_then(|v| v.as_str()) == Some(session.id.as_str())));
}

#[tokio::test]
async fn session_for_task_returns_session_or_null() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task_with = create_test_task(&db, &project.id, &epic.id).await;
    let task_without = create_test_task(&db, &project.id, &epic.id).await;
    let session = create_test_session(&db, &project.id, &task_with.id).await;
    let app = create_test_app();

    let found = mcp_call_tool(
        &app,
        "test-session-for-task-found",
        "session_for_task",
        json!({ "task_id": task_with.id, "project": project.path }),
    )
    .await;
    assert_eq!(found.get("error"), None);
    assert_eq!(found.get("id").and_then(|v| v.as_str()), Some(session.id.as_str()));

    let missing = mcp_call_tool(
        &app,
        "test-session-for-task-missing",
        "session_for_task",
        json!({ "task_id": task_without.id, "project": project.path }),
    )
    .await;
    assert_eq!(missing.get("error"), None);
    assert!(missing.get("id").is_none());
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

    let app = create_test_app();
    let payload = mcp_call_tool(
        &app,
        "test-task-timeline",
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
    let app = create_test_app();

    let payload = mcp_call_tool(
        &app,
        "test-task-timeline-missing",
        "task_timeline",
        json!({ "task_id": "missing-task", "project": project.path }),
    )
    .await;

    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert!(payload.get("sessions").is_none());
    assert!(payload.get("messages").is_none());
}
