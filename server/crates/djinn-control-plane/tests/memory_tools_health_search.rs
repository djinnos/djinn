//! Focused `memory_health` and search-ranking MCP contract tests.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use serde_json::json;

#[tokio::test]
async fn mcp_memory_health_orphans_and_broken_links_shapes() {
    let harness = McpTestHarness::new().await;
    let project = "test/mcp-memory-health";

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Source", "content": "[[Missing Target]]", "reason": "seed health and search fixture", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");

    let health = harness
        .call_tool("memory_health", json!({"project": project}))
        .await
        .expect("memory_health should dispatch");
    assert!(health.get("orphan_note_count").is_some());
    assert!(health.get("broken_link_count").is_some());
    assert!(health.get("low_confidence_note_count").is_some());
    assert!(health.get("stale_note_count").is_some());

    let orphans = harness
        .call_tool("memory_orphans", json!({"project": project}))
        .await
        .expect("memory_orphans should dispatch");
    assert!(orphans["orphans"].is_array());

    let broken = harness
        .call_tool("memory_broken_links", json!({"project": project}))
        .await
        .expect("memory_broken_links should dispatch");
    assert!(broken["broken_links"].is_array());
}

#[tokio::test]
async fn no_regression_memory_search_ranking_notes_only() {
    let harness = McpTestHarness::new().await;
    let (project_row, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = project_row.slug();

    let proposal = harness
        .call_tool(
            "proposal_create",
            json!({"title": "Search Excluded Proposal", "body": "rust rust rust"}),
        )
        .await
        .expect("proposal_create should dispatch");
    let _proposal_id = proposal["id"].as_str().expect("proposal id").to_string();

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Rust Note One", "content": "rust memory test", "reason": "seed health and search fixture", "type": "reference"}),
        )
        .await
        .expect("memory_write one should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Rust Note Two", "content": "another rust note", "reason": "seed health and search fixture", "type": "adr"}),
        )
        .await
        .expect("memory_write two should dispatch");

    let searched = harness
        .call_tool(
            "memory_search",
            json!({"project": project, "query": "rust", "limit": 10}),
        )
        .await
        .expect("memory_search should dispatch");

    let results = searched["results"]
        .as_array()
        .expect("results should be an array");
    let note_results: Vec<_> = results
        .iter()
        .filter(|r| {
            r.get("note_type")
                .and_then(|v| v.as_str())
                .is_some_and(|nt| nt != "proposal")
        })
        .collect();
    assert!(
        note_results.len() >= 2,
        "should find at least 2 notes (got {}): {results:?}",
        note_results.len()
    );

    for result in &note_results {
        assert!(
            result.get("note_type").is_some(),
            "every note result should have note_type: {result:?}"
        );
        assert!(
            result.get("folder").is_some(),
            "every note result should have folder: {result:?}"
        );
    }
}
