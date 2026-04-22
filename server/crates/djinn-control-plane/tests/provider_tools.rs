//! Contract tests for `provider_*` + `model_health` MCP tools.
//!
//! Migrated from `server/src/mcp_contract_tests/provider_tools.rs`.  The
//! harness' `StubRuntime::persist_model_health_state` is a no-op, so
//! `model_health` mutation-shaped tests still return the documented error
//! envelopes when required fields are missing.

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_provider::repos::CredentialRepository;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_catalog_returns_expected_shape() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool("provider_catalog", json!({}))
        .await
        .expect("provider_catalog should dispatch");
    let providers = result["providers"].as_array().expect("providers array");
    assert!(!providers.is_empty());
    assert!(providers[0].get("id").is_some());
    assert!(providers[0].get("name").is_some());
    assert!(result.get("total").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_returns_models_for_valid_provider_and_error_for_unknown() {
    let harness = McpTestHarness::new().await;

    let valid = harness
        .call_tool("provider_models", json!({"provider_id":"openai"}))
        .await
        .expect("provider_models should dispatch");
    assert_eq!(valid["provider_id"], "openai");
    assert!(
        valid["models"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    );

    let unknown = harness
        .call_tool(
            "provider_models",
            json!({"provider_id":"no-such-provider"}),
        )
        .await
        .expect("provider_models should dispatch");
    assert_eq!(unknown["total"], 0);
    assert!(unknown["models"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_connected_returns_only_seeded_provider() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let result = harness
        .call_tool("provider_connected", json!({}))
        .await
        .expect("provider_connected should dispatch");
    let providers = result["providers"].as_array().expect("providers array");
    assert!(!providers.is_empty());
    assert!(
        providers
            .iter()
            .all(|p| p["connected"].as_bool().unwrap_or(false))
    );
    assert!(providers.iter().any(|p| p["id"] == "openai"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_filters_to_connected_provider_models() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let result = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected should dispatch");
    let models = result["models"].as_array().expect("models array");
    assert!(!models.is_empty());
    assert!(models.iter().all(|m| m["provider_id"] == "openai"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_model_lookup_returns_found_and_not_found_shapes() {
    let harness = McpTestHarness::new().await;

    let found = harness
        .call_tool(
            "provider_model_lookup",
            json!({"model_id":"openai/gpt-4o-mini"}),
        )
        .await
        .expect("provider_model_lookup should dispatch");
    assert!(found["found"].as_bool().unwrap_or(false));
    assert!(found.get("model").is_some());

    let not_found = harness
        .call_tool(
            "provider_model_lookup",
            json!({"model_id":"nope/unknown-model"}),
        )
        .await
        .expect("provider_model_lookup should dispatch");
    assert!(!not_found["found"].as_bool().unwrap_or(true));
    assert!(not_found["model"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_health_status_and_param_validation_shapes() {
    let harness = McpTestHarness::new().await;

    let status = harness
        .call_tool("model_health", json!({"action":"status"}))
        .await
        .expect("model_health status should dispatch");
    assert_eq!(status["action"], "status");
    assert!(status["models"].is_array());

    let reset_err = harness
        .call_tool("model_health", json!({"action":"reset"}))
        .await
        .expect("model_health reset should dispatch");
    assert!(reset_err["error"].as_str().is_some());

    let enable_err = harness
        .call_tool("model_health", json!({"action":"enable"}))
        .await
        .expect("model_health enable should dispatch");
    assert!(enable_err["error"].as_str().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_remove_builtin_returns_error_shape() {
    let harness = McpTestHarness::new().await;

    let removed = harness
        .call_tool("provider_remove", json!({"provider_id":"openai"}))
        .await
        .expect("provider_remove should dispatch");
    assert!(removed.get("error").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_validate_returns_error_shape_without_real_key() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool(
            "provider_validate",
            json!({"provider_id":"openai","base_url":"https://api.openai.com/v1","api_key":"sk-invalid"}),
        )
        .await
        .expect("provider_validate should dispatch");
    assert!(result.get("ok").is_some());
    assert!(result.get("error_kind").is_some());
    assert!(result.get("error").is_some());
    assert!(result.get("http_status").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_oauth_start_returns_error_shape_when_not_configured_or_invalid() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool(
            "provider_oauth_start",
            json!({"provider_id":"no-such-provider"}),
        )
        .await
        .expect("provider_oauth_start should dispatch");
    assert!(!result["ok"].as_bool().unwrap_or(true));
    assert!(result["error"].as_str().is_some());
    assert!(result.get("oauth_supported").is_some());
}
