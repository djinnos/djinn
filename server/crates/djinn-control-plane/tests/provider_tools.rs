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
        .call_tool("provider_models", json!({"provider_id":"no-such-provider"}))
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
    // G3: a 404-style miss must carry the structured tool-error envelope so the
    // agent can branch on status instead of re-guessing the same bad id.
    let env = &not_found["error"];
    assert!(
        env.is_object(),
        "expected structured error envelope: {not_found}"
    );
    assert_eq!(env["status"], "404");
    assert_eq!(env["method"], "provider_model_lookup");
    assert_eq!(env["path"], "nope/unknown-model");
    assert!(env["error"].as_str().unwrap().contains("not found"));
    assert!(
        env["hint"]
            .as_str()
            .unwrap()
            .contains("provider_models_connected"),
        "hint should point at the recovery tool: {env}"
    );

    // Backward compatibility: the success path must NOT carry an error envelope.
    assert!(found.get("error").is_none() || found["error"].is_null());
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

// ── Org AI policy: subscription allow/block enforcement (slice 5 of p8py) ──────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_blocks_subscription_from_connected_and_validation() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    // Connect a subscription provider so it would otherwise appear connected.
    CredentialRepository::new(db, EventBus::noop())
        .set("minimax-coding-plan", "MINIMAX_API_KEY", "sk-sub")
        .await
        .unwrap();

    // Baseline: the subscription is connected and a model on it validates.
    let connected_before = harness
        .call_tool("provider_connected", json!({}))
        .await
        .expect("provider_connected dispatch");
    let has_sub_before = connected_before["providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"] == "minimax-coding-plan");
    assert!(
        has_sub_before,
        "connected subscription should be visible before any block"
    );
    harness
        .state()
        .validate_models_for_user(&["minimax-coding-plan/MiniMax-M3".to_string()], None)
        .await
        .expect("model on connected subscription validates before block");

    // Admin blocks the subscription. (No user context in the harness → the
    // admin gate is open, matching the credential-repo trusted-path convention.)
    let set = harness
        .call_tool(
            "org_policy_set",
            json!({"blocked_subscriptions": ["minimax-coding-plan"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "org_policy_set ok");
    assert!(
        set["blocked_subscriptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "minimax-coding-plan"),
        "blocked set persisted"
    );

    // Enforcement 1: hidden from provider_connected.
    let connected_after = harness
        .call_tool("provider_connected", json!({}))
        .await
        .expect("provider_connected dispatch");
    assert!(
        !connected_after["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "minimax-coding-plan"),
        "blocked subscription must be hidden from provider_connected"
    );

    // Enforcement 2: hidden from provider_catalog.
    let catalog_after = harness
        .call_tool("provider_catalog", json!({}))
        .await
        .expect("provider_catalog dispatch");
    assert!(
        !catalog_after["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "minimax-coding-plan"),
        "blocked subscription must be hidden from provider_catalog"
    );

    // Enforcement 3: hidden from provider_models_connected.
    let models_after = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected dispatch");
    assert!(
        !models_after["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["provider_id"] == "minimax-coding-plan"),
        "blocked subscription's models must be hidden from provider_models_connected"
    );

    // Enforcement 4: validation rejects selecting a model on the blocked sub.
    let err = harness
        .state()
        .validate_models_for_user(&["minimax-coding-plan/MiniMax-M3".to_string()], None)
        .await
        .expect_err("model on blocked subscription must be rejected");
    assert!(
        err.contains("blocked by org policy"),
        "rejection should mention org policy, got: {err}"
    );

    // Admin API keys are never governed: a non-subscription id passed to the
    // blocklist is dropped, and openai stays connectable.
    let set_apikey = harness
        .call_tool(
            "org_policy_set",
            json!({"blocked_subscriptions": ["openai"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(
        set_apikey["blocked_subscriptions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "non-subscription provider must not be storable in the blocklist"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_get_reports_jurisdiction_and_lock_default() {
    let harness = McpTestHarness::new().await;
    let result = harness
        .call_tool("org_policy_get", json!({}))
        .await
        .expect("org_policy_get dispatch");
    assert!(result["ok"].as_bool().unwrap_or(false));
    assert_eq!(result["lock_level"], "flexible");
    assert_eq!(
        result["additional_recommended_model_ids"],
        json!([]),
        "fresh org policy should expose an empty additional-recommended override list"
    );
    assert_eq!(
        result["demoted_recommended_model_ids"],
        json!([]),
        "fresh org policy should expose an empty demoted-recommended override list"
    );
    // The subscription table carries a jurisdiction per row.
    if let Some(first) = result["subscriptions"].as_array().and_then(|a| a.first()) {
        let j = first["jurisdiction"].as_str().unwrap_or("");
        assert!(
            matches!(j, "us" | "eu" | "cn" | "other"),
            "jurisdiction must be one of the known buckets, got {j}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_set_round_trips_recommended_model_overrides_and_preserves_omitted_fields() {
    let harness = McpTestHarness::new().await;

    let set = harness
        .call_tool(
            "org_policy_set",
            json!({
                "additional_recommended_model_ids": [
                    "anthropic/claude-sonnet-test-override",
                    "fireworks-ai/accounts/fireworks/models/test-recommended"
                ],
                "demoted_recommended_model_ids": ["openai/gpt-5.5"],
            }),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "org_policy_set ok");
    assert_eq!(
        set["additional_recommended_model_ids"],
        json!([
            "anthropic/claude-sonnet-test-override",
            "fireworks-ai/accounts/fireworks/models/test-recommended"
        ]),
        "additional overrides should persist on set"
    );
    assert_eq!(
        set["demoted_recommended_model_ids"],
        json!(["openai/gpt-5.5"]),
        "demoted overrides should persist on set"
    );

    let get = harness
        .call_tool("org_policy_get", json!({}))
        .await
        .expect("org_policy_get dispatch");
    assert!(get["ok"].as_bool().unwrap_or(false), "org_policy_get ok");
    assert_eq!(
        get["additional_recommended_model_ids"], set["additional_recommended_model_ids"],
        "additional overrides should round-trip through org_policy_get"
    );
    assert_eq!(
        get["demoted_recommended_model_ids"], set["demoted_recommended_model_ids"],
        "demoted overrides should round-trip through org_policy_get"
    );

    let patch_without_overrides = harness
        .call_tool("org_policy_set", json!({"lock_level": "locked"}))
        .await
        .expect("org_policy_set patch dispatch");
    assert!(
        patch_without_overrides["ok"].as_bool().unwrap_or(false),
        "org_policy_set patch ok"
    );
    assert_eq!(
        patch_without_overrides["additional_recommended_model_ids"],
        set["additional_recommended_model_ids"],
        "omitting additional_recommended_model_ids should preserve the saved list"
    );
    assert_eq!(
        patch_without_overrides["demoted_recommended_model_ids"],
        set["demoted_recommended_model_ids"],
        "omitting demoted_recommended_model_ids should preserve the saved list"
    );

    let get_after_patch = harness
        .call_tool("org_policy_get", json!({}))
        .await
        .expect("org_policy_get after patch dispatch");
    assert_eq!(
        get_after_patch["additional_recommended_model_ids"],
        set["additional_recommended_model_ids"],
        "preserved additional overrides should remain visible after patch"
    );
    assert_eq!(
        get_after_patch["demoted_recommended_model_ids"], set["demoted_recommended_model_ids"],
        "preserved demoted overrides should remain visible after patch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_subscription_enumeration_includes_codex_chinese_and_dedupes_copilot() {
    let harness = McpTestHarness::new().await;
    let result = harness
        .call_tool("org_policy_get", json!({}))
        .await
        .expect("org_policy_get dispatch");
    let subs = result["subscriptions"].as_array().expect("subscriptions");

    let by_id = |id: &str| subs.iter().find(|s| s["id"] == id);

    // ChatGPT/Codex is a governable subscription, classified US — even though it
    // is a merged child the catalog otherwise hides under `openai`.
    let codex = by_id("chatgpt_codex").expect("chatgpt_codex must be enumerated");
    assert_eq!(codex["jurisdiction"], "us", "codex is US-hosted");

    // Connected Chinese subs are classified CN.
    for cn_id in ["minimax-coding-plan", "kimi-for-coding", "zai-coding-plan"] {
        let item = by_id(cn_id).unwrap_or_else(|| panic!("{cn_id} must be enumerated"));
        assert_eq!(item["jurisdiction"], "cn", "{cn_id} should be China-hosted");
    }

    // The two GitHub Copilot ids (`github-copilot` + `githubcopilot`) collapse to
    // exactly one row.
    let copilot_count = subs
        .iter()
        .filter(|s| {
            s["id"]
                .as_str()
                .map(|id| {
                    let canon: String = id
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .flat_map(|c| c.to_lowercase())
                        .collect();
                    canon == "githubcopilot"
                })
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        copilot_count, 1,
        "GitHub Copilot must be de-duplicated to one row"
    );

    // Non-subscription API providers are never listed.
    assert!(
        by_id("openai").is_none(),
        "plain openai api key is not governed"
    );
    assert!(by_id("anthropic").is_none());
    assert!(by_id("fireworks-ai").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_blocks_codex_subscription_via_openai_namespaced_models() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    // Connect the openai provider so the connectivity check passes; the test
    // then isolates the org-policy gate (codex block) from connectivity.
    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    // Baseline: a codex model (surfaced under the openai namespace) validates.
    harness
        .state()
        .validate_models_for_user(&["openai/gpt-5.3-codex".to_string()], None)
        .await
        .expect("codex model validates before block");

    // Admin blocks the ChatGPT/Codex subscription by its own identity.
    let set = harness
        .call_tool(
            "org_policy_set",
            json!({"blocked_subscriptions": ["chatgpt_codex"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(
        set["blocked_subscriptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "chatgpt_codex"),
        "codex sub must be storable in the blocklist (it is a subscription)"
    );

    // Enforcement: a codex model namespaced `openai/...` is now rejected, because
    // it resolves to the blocked chatgpt_codex subscription identity.
    let err = harness
        .state()
        .validate_models_for_user(&["openai/gpt-5.3-codex".to_string()], None)
        .await
        .expect_err("codex model must be rejected once the sub is blocked");
    assert!(
        err.contains("blocked by org policy"),
        "rejection should mention org policy, got: {err}"
    );

    // A plain openai API-key model is NOT governed by the codex block.
    harness
        .state()
        .validate_models_for_user(&["openai/gpt-5.5".to_string()], None)
        .await
        .expect("plain openai api-key model stays ungoverned");
}
