//! Contract tests for `board_*` MCP tools.
//!
//! Only `board_health` migrated — it only needs DB-backed tasks/notes.  The
//! `board_reconcile` test stays in `djinn-server` because it requires the
//! real coordinator and slot-pool actors (our harness stubs those).

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_db::NoteRepository;
use serde_json::json;

#[tokio::test]
async fn board_health_with_no_pool_returns_response_shape() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let notes = NoteRepository::new(harness.db().clone(), EventBus::noop());
    notes
        .create_db_note(
            &project.id,
            "Board Health",
            "Planner-visible note",
            "reference",
            "[]",
        )
        .await
        .expect("insert note for memory health summary");

    let response = harness
        .call_tool("board_health", json!({ "project": project.path }))
        .await
        .expect("board_health should dispatch");

    assert!(response.get("stale_tasks").is_some());
    assert!(response.get("epic_stats").is_some());
    assert!(response.get("review_queue").is_some());
    assert!(response.get("memory_health").is_some());
    assert!(response.get("stale_threshold_hours").is_some());
    assert_eq!(response["memory_health"]["total_notes"], 1);
    assert!(response["memory_health"].get("broken_link_count").is_some());
    assert!(response["memory_health"].get("orphan_note_count").is_some());
    assert!(
        response["memory_health"]
            .get("duplicate_cluster_count")
            .is_some()
    );
    assert!(
        response["memory_health"]
            .get("low_confidence_note_count")
            .is_some()
    );
    assert!(response["memory_health"].get("stale_note_count").is_some());
}
