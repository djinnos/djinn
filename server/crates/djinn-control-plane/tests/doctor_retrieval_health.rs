//! Integration coverage for the production retrieval zero-result Doctor check.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::doctor::{RETRIEVAL_ZERO_RESULT_NAME, checks::retrieval::RetrievalHealthConfig};
use serde_json::json;

async fn harness() -> McpTestHarness {
    let harness = McpTestHarness::new().await;
    djinn_db::test_support::ensure_doctor_findings_schema(harness.db()).await;
    harness
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_run_retrieval_check_uses_coordinator_selected_only_response() {
    let harness = harness().await;

    let response = harness
        .call_tool(
            "doctor_run",
            json!({"check_names": [RETRIEVAL_ZERO_RESULT_NAME]}),
        )
        .await
        .expect("dispatch doctor_run");
    assert_eq!(response["ok"], true);
    // An explicit retrieval-only request must not accidentally execute every
    // ordinary globally registered check.
    let results = response["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["check"]["name"], RETRIEVAL_ZERO_RESULT_NAME);
    assert_eq!(results[0]["ran"], true);
    assert!(
        results[0]["findings"]
            .as_array()
            .expect("finding entries")
            .is_empty(),
        "the MCP harness must not recreate the removed private retrieval source"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_retrieval_config_drives_memory_health_without_doctor_prefetch() {
    // This deliberately differs from every default. The one config is injected
    // through McpState::with_enrichment by the harness and both production MCP
    // paths below must read that state-held value.
    let config = RetrievalHealthConfig::new(72, 0.75, 7).expect("valid non-default config");
    let db = djinn_db::Database::open_in_memory().expect("open test database");
    let harness = McpTestHarness::from_db_with_retrieval_config(db, config);
    djinn_db::test_support::ensure_doctor_findings_schema(harness.db()).await;
    let project = common::create_test_project(harness.db()).await;

    let health = harness
        .call_tool("memory_health", json!({"project": project.slug()}))
        .await
        .expect("dispatch memory_health");
    assert_eq!(health["retrieval"]["config_window_hours"], 72);
    assert!(health["retrieval"]["persisted"]["window_start"].is_string());
    assert!(health["retrieval"]["persisted"]["window_end"].is_string());
    assert!(
        health["retrieval"]["persisted"]["groups"]
            .as_array()
            .expect("taxonomy-v1 groups")
            .is_empty()
    );

    let response = harness
        .call_tool(
            "doctor_run",
            json!({"check_names": [RETRIEVAL_ZERO_RESULT_NAME]}),
        )
        .await
        .expect("dispatch doctor_run");
    assert_eq!(response["ok"], true);
    let findings = response["results"][0]["findings"]
        .as_array()
        .expect("retrieval finding entries");
    assert!(findings.is_empty());
}
