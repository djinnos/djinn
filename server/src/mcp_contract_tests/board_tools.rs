//! Remaining board-tool contract tests.
//!
//! `board_health` migrated to `djinn-control-plane/tests/board_tools.rs`; only
//! `board_reconcile` stays here because it exercises the real coordinator +
//! slot-pool actors (the control-plane harness stubs those).

use serde_json::json;

use crate::events::EventBus;
use crate::test_helpers::{
    create_test_db, create_test_epic, create_test_project, create_test_task,
    initialize_mcp_session, mcp_call_tool,
};
use djinn_db::TaskRepository;

#[tokio::test]
async fn board_reconcile_releases_stuck_in_progress_without_active_session() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let tasks = TaskRepository::new(db.clone(), EventBus::noop());
    tasks
        .set_status(&task.id, "in_progress")
        .await
        .expect("set task in_progress");
    tasks
        .set_updated_at(&task.id, "2020-01-01T00:00:00.000Z")
        .await
        .expect("age task beyond stale threshold");

    let state =
        crate::server::AppState::new(db.clone(), tokio_util::sync::CancellationToken::new());
    state.initialize_agents().await;
    let app = crate::server::router(state);
    let session_id = initialize_mcp_session(&app).await;

    let response = mcp_call_tool(
        &app,
        &session_id,
        "board_reconcile",
        json!({ "project": project.path }),
    )
    .await;

    assert!(response.get("healed_tasks").is_some());

    let refreshed = tasks
        .get(&task.id)
        .await
        .expect("fetch task status")
        .expect("task row missing");
    assert_eq!(refreshed.status, "open");
}
