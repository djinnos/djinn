//! Integration tests for the `pr_review_context` meta-tool.
//!
//! The tests run against the stub `RepoGraphOps` — the goal is to exercise the
//! dispatch + serialisation path, not to evaluate graph analysis quality.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn pr_review_context_on_empty_graph_returns_limitations_note() {
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    let out = harness
        .call_tool(
            "pr_review_context",
            json!({
                "project": project.slug(),
                "changed_ranges": [
                    {"file": "nonexistent.rs", "start_line": 1, "end_line": 10}
                ],
            }),
        )
        .await
        .expect("pr_review_context should dispatch");
    // touched_symbols is empty because the file isn't in the graph (stub returns empty).
    assert_eq!(out["changed_ranges_count"], 1);
    assert!(
        !out["limitations_note"].as_str().unwrap_or_default().is_empty(),
        "limitations_note must be present"
    );
}
