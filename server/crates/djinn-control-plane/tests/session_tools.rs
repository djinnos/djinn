//! Contract tests for `session_*` + `task_timeline` MCP tools.
//!
//! Migrated from `server/src/mcp_contract_tests/session_tools.rs`.  The two
//! `*_returns_error_without_pool` tests remain in the server crate because
//! the stub `SlotPoolOps` we ship in the harness returns `Some(..)` with
//! query methods returning empties — it therefore does NOT surface the
//! "slot pool actor not initialized" error the tests assert on.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_db::SessionMessageRepository;
use serde_json::json;

#[tokio::test]
async fn session_list_returns_empty_for_task_without_sessions() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;

    let payload = harness
        .call_tool(
            "session_list",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("session_list should dispatch");
    assert_eq!(payload.get("error"), None);
    assert_eq!(
        payload.get("task_id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert!(
        payload
            .get("sessions")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn session_list_filters_by_project_and_task() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project_a = common::create_test_project(db).await;
    let epic_a = common::create_test_epic(db, &project_a.id).await;
    let task_a1 = common::create_test_task(db, &project_a.id, &epic_a.id).await;
    let task_a2 = common::create_test_task(db, &project_a.id, &epic_a.id).await;
    let project_b = common::create_test_project(db).await;
    let epic_b = common::create_test_epic(db, &project_b.id).await;
    let task_b1 = common::create_test_task(db, &project_b.id, &epic_b.id).await;
    let _s_a1_1 = common::create_test_session(db, &project_a.id, &task_a1.id).await;
    let _s_a1_2 = common::create_test_session(db, &project_a.id, &task_a1.id).await;
    let _s_a2 = common::create_test_session(db, &project_a.id, &task_a2.id).await;
    let _s_b1 = common::create_test_session(db, &project_b.id, &task_b1.id).await;

    let payload = harness
        .call_tool(
            "session_list",
            json!({ "task_id": task_a1.id, "project": project_a.slug() }),
        )
        .await
        .expect("session_list should dispatch");
    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .all(|s| s.get("task_id").and_then(|v| v.as_str()) == Some(task_a1.id.as_str()))
    );
    assert!(
        sessions
            .iter()
            .all(|s| s.get("project_id").and_then(|v| v.as_str()) == Some(project_a.id.as_str()))
    );
}

#[tokio::test]
async fn session_show_returns_full_shape_with_tokens() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let session = common::create_test_session(db, &project.id, &task.id).await;

    let payload = harness
        .call_tool(
            "session_show",
            json!({ "id": session.id, "project": project.slug() }),
        )
        .await
        .expect("session_show should dispatch");
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
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let payload = harness
        .call_tool(
            "session_show",
            json!({ "id": "missing-session-id", "project": project.slug() }),
        )
        .await
        .expect("session_show should dispatch");
    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert_eq!(payload.get("id"), None);
}

#[tokio::test]
async fn task_timeline_returns_chronological_session_and_message_history() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let s1 = common::create_test_session(db, &project.id, &task.id).await;
    let s2 = common::create_test_session(db, &project.id, &task.id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
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

    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");
    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(messages.len(), 2);
    let ts0 = messages[0]
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap();
    let ts1 = messages[1]
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(ts0 <= ts1);
}

#[tokio::test]
async fn task_timeline_not_found_returns_error_shape() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": "missing-task", "project": project.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");
    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert!(payload.get("sessions").is_none());
    assert!(payload.get("messages").is_none());
}

#[tokio::test]
async fn session_messages_returns_messages_for_valid_session_id() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let sess = common::create_test_session(db, &project.id, &task.id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
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

    let payload = harness
        .call_tool(
            "session_messages",
            json!({ "id": sess.id, "project": project.slug() }),
        )
        .await
        .expect("session_messages should dispatch");
    assert_eq!(payload.get("error"), None);
    let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].get("role").and_then(|v| v.as_str()),
        Some("user")
    );
}
