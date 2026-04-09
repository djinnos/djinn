use serde_json::json;

use crate::test_helpers::{
    create_test_app_with_db, create_test_db, create_test_epic, create_test_project,
    create_test_task, initialize_mcp_session, mcp_call_tool,
};

#[tokio::test]
async fn board_health_with_no_pool_returns_response_shape() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let response = mcp_call_tool(
        &app,
        &session_id,
        "board_health",
        json!({ "project": project.path }),
    )
    .await;

    assert!(response.get("stale_tasks").is_some());
    assert!(response.get("epic_stats").is_some());
    assert!(response.get("review_queue").is_some());
    assert!(response.get("stale_threshold_hours").is_some());
}

#[tokio::test]
async fn board_reconcile_releases_stuck_in_progress_without_active_session() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = ?1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .expect("set task in_progress");
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
        .bind(&task.id)
        .execute(db.pool())
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

    let status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?1")
        .bind(&task.id)
        .fetch_one(db.pool())
        .await
        .expect("fetch task status");
    assert_eq!(status, "open");
}
