//! Contract tests for `provider_*` + `model_health` MCP tools.
//!
//! Migrated from `server/src/mcp_contract_tests/provider_tools.rs`.  The
//! harness' `StubRuntime::persist_model_health_state` is a no-op, so
//! `model_health` mutation-shaped tests still return the documented error
//! envelopes when required fields are missing.
//!
//! Recommended-model override policy coverage exercises the effective
//! `recommended` flag on `provider_models_connected` outputs.

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_core::events::EventBus;
use djinn_db::OrgAiPolicyRepository;
use djinn_db::repositories::user::UserRepository;
use djinn_provider::repos::CredentialRepository;
use serde_json::json;
use std::collections::HashMap;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_blocks_subscription_from_connected_and_validation() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("minimax-coding-plan", "MINIMAX_API_KEY", "sk-sub")
        .await
        .unwrap();

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
        .validate_models_for_user(&["minimax-coding-plan/MiniMax-M2.5".to_string()], None)
        .await
        .expect("model on connected subscription validates before block");

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

    let err = harness
        .state()
        .validate_models_for_user(&["minimax-coding-plan/MiniMax-M2.5".to_string()], None)
        .await
        .expect_err("model on blocked subscription must be rejected");
    assert!(
        err.contains("blocked by org policy"),
        "rejection should mention org policy, got: {err}"
    );

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

    let codex = by_id("chatgpt_codex").expect("chatgpt_codex must be enumerated");
    assert_eq!(codex["jurisdiction"], "us", "codex is US-hosted");

    for cn_id in ["minimax-coding-plan", "kimi-for-coding", "zai-coding-plan"] {
        let item = by_id(cn_id).unwrap_or_else(|| panic!("{cn_id} must be enumerated"));
        assert_eq!(item["jurisdiction"], "cn", "{cn_id} should be China-hosted");
    }

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

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    harness
        .state()
        .validate_models_for_user(&["openai/gpt-5.3-codex".to_string()], None)
        .await
        .expect("codex model validates before block");

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

    let err = harness
        .state()
        .validate_models_for_user(&["openai/gpt-5.3-codex".to_string()], None)
        .await
        .expect_err("codex model must be rejected once the sub is blocked");
    assert!(
        err.contains("blocked by org policy"),
        "rejection should mention org policy, got: {err}"
    );

    harness
        .state()
        .validate_models_for_user(&["openai/gpt-5.2".to_string()], None)
        .await
        .expect("plain openai api-key model stays ungoverned");
}

// ── Org AI policy: recommended-model override policy on connected outputs ─────

async fn connected_models_recommended_map(harness: &McpTestHarness) -> HashMap<String, bool> {
    let result = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected dispatch");
    result["models"]
        .as_array()
        .expect("models array")
        .iter()
        .map(|m| {
            (
                m["id"].as_str().expect("model id").to_string(),
                m["recommended"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_demotes_baseline_recommended_model() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let before = connected_models_recommended_map(&harness).await;
    let gpt_5_3_codex = "openai/gpt-5.3-codex";
    assert!(
        before.contains_key(gpt_5_3_codex),
        "{gpt_5_3_codex} should be present in connected output, got ids: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        *before.get(gpt_5_3_codex).unwrap(),
        "{gpt_5_3_codex} should be baseline recommended before demotion"
    );

    let set = harness
        .call_tool(
            "org_policy_set",
            json!({"demoted_recommended_model_ids": [gpt_5_3_codex]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "org_policy_set ok");

    let after = connected_models_recommended_map(&harness).await;
    assert!(
        !after.get(gpt_5_3_codex).copied().unwrap_or(true),
        "demoted baseline recommended model should surface recommended=false"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_promotes_non_baseline_model() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let before = connected_models_recommended_map(&harness).await;
    let gpt_5_2 = "openai/gpt-5.2";
    assert!(
        before.contains_key(gpt_5_2),
        "gpt-5.2 should be present in connected output, got ids: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        !before.get(gpt_5_2).copied().unwrap_or(false),
        "gpt-5.2 should not be baseline recommended before promotion"
    );

    let set = harness
        .call_tool(
            "org_policy_set",
            json!({"additional_recommended_model_ids": [gpt_5_2]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "org_policy_set ok");

    let after = connected_models_recommended_map(&harness).await;
    assert!(
        *after
            .get(gpt_5_2)
            .expect("gpt-5.2 must still be present in catalog output"),
        "additional override should promote gpt-5.2 to recommended=true"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_ignores_override_ids_missing_from_catalog() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let set = harness
        .call_tool(
            "org_policy_set",
            json!({
                "additional_recommended_model_ids": ["openai/nonexistent-model"],
                "demoted_recommended_model_ids": ["openai/also-nonexistent"],
            }),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "org_policy_set ok");

    let result = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected dispatch");
    let models = result["models"].as_array().expect("models array");
    assert!(
        !models.iter().any(|m| m["id"] == "openai/nonexistent-model"),
        "additional override missing from catalog must not synthesize a model"
    );
    assert!(
        !models.iter().any(|m| m["id"] == "openai/also-nonexistent"),
        "demoted override missing from catalog must not synthesize a model"
    );
    assert!(result.get("error").is_none() || result["error"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_overlap_demotion_wins_for_legacy_corrupt_policy() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db.clone(), EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let repo = OrgAiPolicyRepository::new(db);
    let mut policy = repo.get().await.expect("load policy");
    policy.additional_recommended_model_ids = vec!["openai/gpt-5.3-codex".to_string()];
    policy.demoted_recommended_model_ids = vec!["openai/gpt-5.3-codex".to_string()];
    repo.set(&policy).await.expect("persist corrupt overlap");

    let result = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected dispatch");
    let gpt_5_3_codex = result["models"]
        .as_array()
        .expect("models array")
        .iter()
        .find(|m| m["id"] == "openai/gpt-5.3-codex")
        .expect("openai/gpt-5.3-codex must remain in output")
        .clone();
    assert!(
        !gpt_5_3_codex["recommended"].as_bool().unwrap_or(true),
        "demotion wins over addition for overlapping ids in corrupt policy"
    );
    assert!(result.get("error").is_none() || result["error"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_merged_child_preserves_full_path_and_uses_surfaced_id() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("chatgpt_codex", "CHATGPT_CODEX_TOKEN", "token")
        .await
        .unwrap();

    let before = connected_models_recommended_map(&harness).await;
    let codex_model = "openai/gpt-5.3-codex";
    assert!(
        before.contains_key(codex_model),
        "{codex_model} should be present in connected output, got ids: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        *before.get(codex_model).unwrap(),
        "codex model should be baseline recommended before demotion"
    );

    let set = harness
        .call_tool(
            "org_policy_set",
            json!({"demoted_recommended_model_ids": [codex_model]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "org_policy_set ok");

    let after = connected_models_recommended_map(&harness).await;
    assert!(
        !after.get(codex_model).copied().unwrap_or(true),
        "demotion must apply to the merged-child surfaced id {codex_model}"
    );
}

// ── Browse-all subscription filtering and merged-child id regressions ───────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_rejects_blocked_and_unsupported_subscription_models() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let model_id = "minimax-coding-plan/MiniMax-M2.5";

    let connected = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected dispatch");
    assert!(
        !connected["models"]
            .as_array()
            .expect("models array")
            .iter()
            .any(|m| m["id"] == model_id),
        "unsupported (disconnected) subscription model must be absent from provider_models_connected"
    );

    let err = harness
        .state()
        .validate_models_for_user(&[model_id.to_string()], None)
        .await
        .expect_err("unsupported subscription model must fail dispatch validation");
    assert!(
        err.contains("haven't connected"),
        "expected disconnected-provider rejection, got: {err}"
    );

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
        "blocked subscription must be persisted"
    );

    let connected_after = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected dispatch after block");
    assert!(
        !connected_after["models"]
            .as_array()
            .expect("models array after block")
            .iter()
            .any(|m| m["id"] == model_id),
        "blocked subscription model must remain absent from provider_models_connected"
    );

    let err_blocked = harness
        .state()
        .validate_models_for_user(&[model_id.to_string()], None)
        .await
        .expect_err("blocked subscription model must fail dispatch validation");
    assert!(
        err_blocked.contains("blocked by org policy"),
        "expected blocked-subscription rejection, got: {err_blocked}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_merged_child_persists_and_validates_full_id() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db.clone(), EventBus::noop())
        .set("chatgpt_codex", "CHATGPT_CODEX_TOKEN", "token")
        .await
        .unwrap();

    let connected = harness
        .call_tool("provider_models_connected", json!({}))
        .await
        .expect("provider_models_connected dispatch");
    let codex_model = connected["models"]
        .as_array()
        .expect("models array")
        .iter()
        .find(|m| m["id"] == "openai/gpt-5.3-codex")
        .expect("merged-child codex model must appear in connected output")
        .clone();
    assert_eq!(
        codex_model["id"], "openai/gpt-5.3-codex",
        "connected output must use the surfaced namespaced id"
    );
    assert_eq!(
        codex_model["provider_id"], "openai",
        "connected output must surface the parent provider id"
    );

    let user_repo = UserRepository::new(db.clone());
    let user = user_repo
        .upsert_from_github(999_004, "merged-child-test-user", None, None)
        .await
        .expect("create test user");
    let user_id = user.id;

    let set = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness
                .call_tool(
                    "user_settings_set",
                    json!({
                        "lanes": {
                            "implement": ["openai/gpt-5.3-codex"]
                        }
                    }),
                )
                .await
        })
        .await
        .expect("user_settings_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "user_settings_set ok");
    let set_lanes = set["lanes"].as_object().expect("set response lanes");
    assert_eq!(
        set_lanes["implement"],
        json!(["openai/gpt-5.3-codex"]),
        "user_settings_set must echo the exact surfaced id"
    );

    let get = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness.call_tool("user_settings_get", json!({})).await
        })
        .await
        .expect("user_settings_get dispatch");
    assert!(get["ok"].as_bool().unwrap_or(false), "user_settings_get ok");
    let get_lanes = get["lanes"].as_object().expect("get response lanes");
    assert_eq!(
        get_lanes["implement"],
        json!(["openai/gpt-5.3-codex"]),
        "user_settings_get must read back the exact surfaced id unchanged"
    );

    harness
        .state()
        .validate_models_for_user(&["openai/gpt-5.3-codex".to_string()], Some(&user_id))
        .await
        .expect("merged-child surfaced id must be dispatch-valid");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_models_connected_overrides_do_not_resurrect_filtered_subscription_models() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db, EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let set = harness
        .call_tool(
            "org_policy_set",
            json!({
                "blocked_subscriptions": ["chatgpt_codex", "minimax-coding-plan"],
                "additional_recommended_model_ids": [
                    "minimax-coding-plan/MiniMax-M2.5",
                    "openai/gpt-5.2"
                ],
                "demoted_recommended_model_ids": ["openai/gpt-5.3-codex"]
            }),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(set["ok"].as_bool().unwrap_or(false), "org_policy_set ok");

    let map = connected_models_recommended_map(&harness).await;
    assert!(
        !map.contains_key("minimax-coding-plan/MiniMax-M2.5"),
        "promotion must not reveal a blocked/disconnected subscription model"
    );
    assert!(
        !*map
            .get("openai/gpt-5.3-codex")
            .expect("openai/gpt-5.3-codex should remain present in connected catalog"),
        "demotion must not hide a connected catalog model"
    );
    assert!(
        *map.get("openai/gpt-5.2")
            .expect("openai/gpt-5.2 should be present in connected catalog"),
        "promotion should still apply to a connected catalog model"
    );
}

// ── Org AI policy: recommended-model override-list validation regressions ──────

fn assert_org_policy_set_error(result: &serde_json::Value, expected_substring: &str) {
    assert!(
        !result["ok"].as_bool().unwrap_or(true),
        "org_policy_set should fail for invalid override input: {result}"
    );
    let error = result["error"].as_str().unwrap_or("");
    assert!(
        error.contains(expected_substring),
        "expected error containing `{expected_substring}`, got: `{error}`"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_set_rejects_raw_local_recommended_model_override_ids() {
    let harness = McpTestHarness::new().await;
    let result = harness
        .call_tool(
            "org_policy_set",
            json!({"additional_recommended_model_ids": ["claude-sonnet-test-override"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert_org_policy_set_error(&result, "no `/` separator");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_set_rejects_malformed_qualified_recommended_model_override_ids() {
    let harness = McpTestHarness::new().await;

    let empty_provider = harness
        .call_tool(
            "org_policy_set",
            json!({"additional_recommended_model_ids": ["/claude-sonnet-test-override"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert_org_policy_set_error(&empty_provider, "empty provider prefix");

    let empty_model = harness
        .call_tool(
            "org_policy_set",
            json!({"additional_recommended_model_ids": ["anthropic/"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert_org_policy_set_error(&empty_model, "empty model id");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_set_rejects_duplicate_recommended_model_override_ids_in_one_list() {
    let harness = McpTestHarness::new().await;
    let result = harness
        .call_tool(
            "org_policy_set",
            json!({
                "additional_recommended_model_ids": [
                    "anthropic/claude-sonnet-test-override",
                    "anthropic/claude-sonnet-test-override"
                ]
            }),
        )
        .await
        .expect("org_policy_set dispatch");
    assert_org_policy_set_error(&result, "duplicate model id");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_set_rejects_recommended_model_override_ids_in_both_lists() {
    let harness = McpTestHarness::new().await;
    let result = harness
        .call_tool(
            "org_policy_set",
            json!({
                "additional_recommended_model_ids": ["openai/gpt-5.5"],
                "demoted_recommended_model_ids": ["openai/gpt-5.5"]
            }),
        )
        .await
        .expect("org_policy_set dispatch");
    assert_org_policy_set_error(&result, "appears in both");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_set_rejects_unknown_provider_recommended_model_override_ids() {
    let harness = McpTestHarness::new().await;
    let result = harness
        .call_tool(
            "org_policy_set",
            json!({"additional_recommended_model_ids": ["nope/unknown-model"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert_org_policy_set_error(&result, "not a known provider");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn org_policy_set_accepts_known_but_disconnected_provider_recommended_model_overrides() {
    let harness = McpTestHarness::new().await;

    let result = harness
        .call_tool(
            "org_policy_set",
            json!({"additional_recommended_model_ids": ["google/gemini-1"]}),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "org_policy_set should accept a known provider even when disconnected: {result}"
    );
    assert!(
        result["error"].is_null(),
        "accepted override should carry no error: {result}"
    );
    assert_eq!(
        result["additional_recommended_model_ids"],
        json!(["google/gemini-1"]),
        "known disconnected provider override should be persisted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_recommended_connected_model_persists_and_validates_including_org_overrides() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    CredentialRepository::new(db.clone(), EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .unwrap();

    let before = connected_models_recommended_map(&harness).await;
    let dotted_non_recommended = "openai/gpt-5.2";
    assert!(
        before.contains_key(dotted_non_recommended),
        "{dotted_non_recommended} should be in connected output, got ids: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        !before.get(dotted_non_recommended).copied().unwrap_or(false),
        "{dotted_non_recommended} should be non-recommended before overrides"
    );
    let baseline_recommended = "openai/gpt-5.3-codex";
    assert!(
        before.contains_key(baseline_recommended),
        "{baseline_recommended} should be in connected output, got ids: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        *before
            .get(baseline_recommended)
            .expect("codex model present"),
        "{baseline_recommended} should be baseline recommended before overrides"
    );

    let user_repo = UserRepository::new(db.clone());
    let user = user_repo
        .upsert_from_github(999_003, "provider-tools-test-user", None, None)
        .await
        .expect("create test user");
    let user_id = user.id;

    let set = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness
                .call_tool(
                    "user_settings_set",
                    json!({
                        "lanes": {
                            "implement": [dotted_non_recommended]
                        }
                    }),
                )
                .await
        })
        .await
        .expect("user_settings_set dispatch");
    assert!(
        set["ok"].as_bool().unwrap_or(false),
        "user_settings_set should accept the non-recommended connected model: {set}"
    );
    let set_lanes = set["lanes"].as_object().expect("set response lanes");
    assert_eq!(
        set_lanes["implement"],
        json!([dotted_non_recommended]),
        "user_settings_set should echo the exact dotted id"
    );

    let get = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness.call_tool("user_settings_get", json!({})).await
        })
        .await
        .expect("user_settings_get dispatch");
    assert!(
        get["ok"].as_bool().unwrap_or(false),
        "user_settings_get should succeed: {get}"
    );
    let get_lanes = get["lanes"].as_object().expect("get response lanes");
    assert_eq!(
        get_lanes["implement"],
        json!([dotted_non_recommended]),
        "user_settings_get should read back the exact dotted id unchanged"
    );

    harness
        .state()
        .validate_models_for_user(&[dotted_non_recommended.to_string()], Some(&user_id))
        .await
        .expect("catalog-present non-recommended id should be dispatch-valid");

    let absent = "openai/not-in-catalog";
    let err = harness
        .state()
        .validate_models_for_user(&[absent.to_string()], Some(&user_id))
        .await
        .expect_err("catalog-absent id should be rejected by dispatch validation");
    assert!(
        err.contains("not available in the connected provider catalog"),
        "expected catalog membership rejection for absent id, got: {err}"
    );

    let rejected_set = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness
                .call_tool("user_settings_set", json!({"lanes": {"plan": [absent]}}))
                .await
        })
        .await
        .expect("user_settings_set rejection dispatch");
    assert!(
        !rejected_set["ok"].as_bool().unwrap_or(true),
        "user_settings_set must reject a catalog-absent id under a connected provider: {rejected_set}"
    );
    assert!(
        rejected_set["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not available in the connected provider catalog"),
        "user_settings_set should report catalog membership rejection: {rejected_set}"
    );

    let policy_set = harness
        .call_tool(
            "org_policy_set",
            json!({
                "additional_recommended_model_ids": [dotted_non_recommended],
                "demoted_recommended_model_ids": [baseline_recommended]
            }),
        )
        .await
        .expect("org_policy_set dispatch");
    assert!(
        policy_set["ok"].as_bool().unwrap_or(false),
        "org_policy_set overrides should succeed: {policy_set}"
    );

    let after = connected_models_recommended_map(&harness).await;
    assert!(
        *after
            .get(dotted_non_recommended)
            .expect("dotted id still present in catalog after promotion"),
        "additional override should promote {dotted_non_recommended} to recommended"
    );
    assert!(
        !after.get(baseline_recommended).copied().unwrap_or(true),
        "demotion should mark {baseline_recommended} as not recommended"
    );

    let set_after = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness
                .call_tool(
                    "user_settings_set",
                    json!({
                        "lanes": {
                            "plan": [baseline_recommended],
                            "implement": [dotted_non_recommended]
                        }
                    }),
                )
                .await
        })
        .await
        .expect("user_settings_set after overrides dispatch");
    assert!(
        set_after["ok"].as_bool().unwrap_or(false),
        "catalog-present models should remain persistable after overrides: {set_after}"
    );
    let lanes_after = set_after["lanes"]
        .as_object()
        .expect("lanes after overrides");
    assert_eq!(
        lanes_after["plan"],
        json!([baseline_recommended]),
        "demoted model should still persist by exact id"
    );
    assert_eq!(
        lanes_after["implement"],
        json!([dotted_non_recommended]),
        "promoted model should still persist by exact id"
    );

    harness
        .state()
        .validate_models_for_user(
            &[
                dotted_non_recommended.to_string(),
                baseline_recommended.to_string(),
            ],
            Some(&user_id),
        )
        .await
        .expect("catalog-present models should remain dispatch-valid after overrides");
}
