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

    // Backward-compatible coarse status fields must remain present.
    assert!(response.get("stale_tasks").is_some());
    assert!(response.get("epic_stats").is_some());
    assert!(response.get("review_queue").is_some());
    assert!(response.get("stale_threshold_hours").is_some());
    // Memory health is no longer embedded in board_health (the planner
    // patrol that consumed it was removed with proposal 1omc); note-health
    // signals live on the dedicated `memory_health` tool.
    assert!(response.get("memory_health").is_none());
}

#[tokio::test]
async fn board_health_returns_additive_liveness_and_stranded_sections() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let response = harness
        .call_tool("board_health", json!({ "project": project.slug() }))
        .await
        .expect("board_health should dispatch");

    // New additive sections produced by the DB-side board_health work in
    // task lke3 — the MCP surface must surface them with default/skip-empty
    // behavior so old DB payloads that pre-date these sections still
    // deserialize (verified implicitly here because the harness has a
    // brand-new DB with no rows, yet the call succeeds).
    let liveness_outcomes = response
        .get("liveness_outcomes")
        .expect("liveness_outcomes section must be present");
    assert_eq!(
        liveness_outcomes.get("total").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert!(liveness_outcomes.get("by_verdict").is_some());
    assert!(
        liveness_outcomes
            .get("recent")
            .and_then(|v| v.as_array())
            .is_some()
    );

    let protocol_violations = response
        .get("protocol_violations")
        .expect("protocol_violations section must be present");
    assert_eq!(
        protocol_violations.get("total").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert!(
        protocol_violations
            .get("recent")
            .and_then(|v| v.as_array())
            .is_some()
    );

    let stranded_ready = response
        .get("stranded_ready")
        .expect("stranded_ready section must be present");
    assert_eq!(
        stranded_ready.get("total").and_then(|v| v.as_i64()),
        Some(0)
    );
    // Base 30-minute threshold from the design contract must be echoed back
    // so clients can interpret severity without hard-coding the ladder.
    assert_eq!(
        stranded_ready
            .get("threshold_minutes")
            .and_then(|v| v.as_i64()),
        Some(30)
    );
    assert!(
        stranded_ready
            .get("findings")
            .and_then(|v| v.as_array())
            .is_some()
    );

    // The coarse status fields still coexist with the additive sections.
    assert!(response.get("stale_tasks").is_some());
    assert!(response.get("epic_stats").is_some());
    assert!(response.get("review_queue").is_some());
    assert!(response.get("stale_threshold_hours").is_some());
}
