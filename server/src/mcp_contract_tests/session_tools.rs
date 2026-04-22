//! Remaining session-tool contract tests.
//!
//! The happy-path + not-found tests for `session_list`, `session_show`,
//! `session_messages`, and `task_timeline` migrated to
//! `djinn-control-plane/tests/session_tools.rs`.  The two
//! `*_returns_error_without_pool` tests stay here: the control-plane harness'
//! `StubSlotPool` returns `Some(..)` with empty-result query methods, so it
//! doesn't surface the "slot pool actor not initialized" error these tests
//! assert on.  `AppState::pool()` in the real server returns `None` until
//! `initialize_agents()` runs, which is exactly what these tests want.

use serde_json::json;

use crate::test_helpers::{
    create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
    create_test_session, create_test_task, initialize_mcp_session, mcp_call_tool,
};

#[tokio::test]
async fn session_active_returns_error_without_pool() {
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
