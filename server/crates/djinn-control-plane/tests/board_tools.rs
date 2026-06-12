//! Contract tests for `board_*` MCP tools.
//!
//! Only `board_health` migrated — it only needs DB-backed tasks/notes.  The
//! `board_reconcile` test stays in `djinn-server` because it requires the
//! real coordinator and slot-pool actors (our harness stubs those).

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn board_health_with_no_pool_returns_response_shape() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    assert!(response.get("stale_tasks").is_some());
    assert!(response.get("epic_stats").is_some());
    assert!(response.get("review_queue").is_some());
    assert!(response.get("stale_threshold_hours").is_some());
    // Memory health is no longer embedded in board_health (the planner
    // patrol that consumed it was removed with proposal 1omc); note-health
    // signals live on the dedicated `memory_health` tool.
    assert!(response.get("memory_health").is_none());
}
