use crate::test_helpers::{
    create_test_app_with_db, create_test_db, initialize_mcp_session, mcp_call_tool,
};
use djinn_provider::repos::CredentialRepository;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_catalog_returns_expected_shape() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let result = mcp_call_tool(&app, &session_id, "provider_catalog", serde_json::json!({})).await;
    let providers = result["providers"].as_array().expect("providers array");
    assert!(!providers.is_empty());
    assert!(providers[0].get("id").is_some());
    assert!(providers[0].get("name").is_some());
    assert!(result.get("total").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_returns_models_for_valid_provider_and_error_for_unknown() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let valid = mcp_call_tool(
        &app,
        &session_id,
        "provider_models",
        serde_json::json!({"provider_id":"openai"}),
    )
    .await;
    assert_eq!(valid["provider_id"], "openai");
    assert!(
        valid["models"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    );

    let unknown = mcp_call_tool(
        &app,
        &session_id,
        "provider_models",
        serde_json::json!({"provider_id":"no-such-provider"}),
    )
    .await;
    assert_eq!(unknown["total"], 0);
    assert!(unknown["models"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_connected_returns_only_seeded_provider() {
    let db = create_test_db();
    let app = create_test_app_with_db(db.clone());
    let session_id = initialize_mcp_session(&app).await;

    CredentialRepository::new(db, crate::events::EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let result = mcp_call_tool(
        &app,
        &session_id,
        "provider_connected",
        serde_json::json!({}),
    )
    .await;
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
    let db = create_test_db();
    let app = create_test_app_with_db(db.clone());
    let session_id = initialize_mcp_session(&app).await;

    CredentialRepository::new(db, crate::events::EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let result = mcp_call_tool(
        &app,
        &session_id,
        "provider_models_connected",
        serde_json::json!({}),
    )
    .await;
    let models = result["models"].as_array().expect("models array");
    assert!(!models.is_empty());
    assert!(models.iter().all(|m| m["provider_id"] == "openai"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_model_lookup_returns_found_and_not_found_shapes() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let found = mcp_call_tool(
        &app,
        &session_id,
        "provider_model_lookup",
        serde_json::json!({"model_id":"openai/gpt-4o-mini"}),
    )
    .await;
    assert!(found["found"].as_bool().unwrap_or(false));
    assert!(found.get("model").is_some());

    let not_found = mcp_call_tool(
        &app,
        &session_id,
        "provider_model_lookup",
        serde_json::json!({"model_id":"nope/unknown-model"}),
    )
    .await;
    assert!(!not_found["found"].as_bool().unwrap_or(true));
    assert!(not_found["model"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_health_status_and_param_validation_shapes() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let status = mcp_call_tool(
        &app,
        &session_id,
        "model_health",
        serde_json::json!({"action":"status"}),
    )
    .await;
    assert_eq!(status["action"], "status");
    assert!(status["models"].is_array());

    let reset_err = mcp_call_tool(
        &app,
        &session_id,
        "model_health",
        serde_json::json!({"action":"reset"}),
    )
    .await;
    assert!(reset_err["error"].as_str().is_some());

    let enable_err = mcp_call_tool(
        &app,
        &session_id,
        "model_health",
        serde_json::json!({"action":"enable"}),
    )
    .await;
    assert!(enable_err["error"].as_str().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_add_custom_and_remove_custom_work() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let added = mcp_call_tool(&app, &session_id, "provider_add_custom", serde_json::json!({"id":"my-custom","name":"My Custom","base_url":"https://example.invalid/v1","env_var":"MY_CUSTOM_API_KEY","seed_models":[{"id":"my-model","name":"My Model"}]})).await;
    assert!(added["ok"].as_bool().unwrap_or(false));
    assert_eq!(added["id"], "my-custom");

    let removed = mcp_call_tool(
        &app,
        &session_id,
        "provider_remove",
        serde_json::json!({"provider_id":"my-custom"}),
    )
    .await;
    assert!(removed["ok"].as_bool().unwrap_or(false));
    assert!(
        removed["custom_provider_deleted"]
            .as_bool()
            .unwrap_or(false)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_remove_builtin_returns_error_shape() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let removed = mcp_call_tool(
        &app,
        &session_id,
        "provider_remove",
        serde_json::json!({"provider_id":"openai"}),
    )
    .await;
    assert!(removed.get("error").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_validate_returns_error_shape_without_real_key() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let result = mcp_call_tool(&app, &session_id, "provider_validate", serde_json::json!({"provider_id":"openai","base_url":"https://api.openai.com/v1","api_key":"sk-invalid"})).await;
    assert!(result.get("ok").is_some());
    assert!(result.get("error_kind").is_some());
    assert!(result.get("error").is_some());
    assert!(result.get("http_status").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_oauth_start_returns_error_shape_when_not_configured_or_invalid() {
    let db = create_test_db();
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let result = mcp_call_tool(
        &app,
        &session_id,
        "provider_oauth_start",
        serde_json::json!({"provider_id":"no-such-provider"}),
    )
    .await;
    assert!(!result["ok"].as_bool().unwrap_or(true));
    assert!(result["error"].as_str().is_some());
    assert!(result.get("oauth_supported").is_some());
}
