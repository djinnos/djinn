//! Contract tests for `credential_*` MCP tools.
//!
//! Migrated from `server/src/mcp_contract_tests/credential_tools.rs`.  These
//! exercise the `CredentialRepository` round-trip through the tool surface —
//! no bridge traits involved, so the harness' in-memory DB is enough.

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_provider::repos::CredentialRepository;
use serde_json::json;

#[tokio::test]
async fn credential_set_success_shape() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();

    let res = harness
        .call_tool(
            "credential_set",
            json!({"provider_id":"anthropic","key_name":"ANTHROPIC_API_KEY","api_key":"secret-1"}),
        )
        .await
        .expect("credential_set should dispatch");

    assert_eq!(res["ok"], true);
    assert_eq!(res["success"], true);
    assert_eq!(res["key_name"], "ANTHROPIC_API_KEY");
    assert!(res["id"].as_str().unwrap_or_default().len() > 8);

    let repo = CredentialRepository::new(db.clone(), EventBus::noop());
    let ciphertext = repo
        .get_encrypted_raw("ANTHROPIC_API_KEY")
        .await
        .unwrap()
        .expect("missing credential row");
    assert!(!ciphertext.is_empty());
    assert_ne!(ciphertext, b"secret-1");

    let decrypted = repo
        .get_decrypted("ANTHROPIC_API_KEY")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decrypted, "secret-1");
}

#[tokio::test]
async fn credential_list_hides_secrets() {
    let harness = McpTestHarness::new().await;

    let _ = harness
        .call_tool(
            "credential_set",
            json!({"provider_id":"openai","key_name":"OPENAI_API_KEY","api_key":"super-secret"}),
        )
        .await
        .expect("credential_set should dispatch");

    let list = harness
        .call_tool("credential_list", json!({}))
        .await
        .expect("credential_list should dispatch");
    let first = list["credentials"].as_array().unwrap().first().unwrap();
    assert_eq!(first["key_name"], "OPENAI_API_KEY");
    assert!(first.get("api_key").is_none());
    assert!(first.get("ciphertext").is_none());
}

#[tokio::test]
async fn credential_set_rejects_org_shared_subscription() {
    let harness = McpTestHarness::new().await;

    // A personal-subscription provider (Kimi coding plan) must never be stored
    // org-shared — sharing one plan across users violates provider ToS.
    let res = harness
        .call_tool(
            "credential_set",
            json!({
                "provider_id":"kimi-for-coding",
                "key_name":"KIMI_API_KEY",
                "api_key":"sk-personal",
                "org_shared": true
            }),
        )
        .await
        .expect("credential_set should dispatch");

    assert_eq!(res["ok"], false);
    assert_eq!(res["success"], false);
    let err = res["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("subscription"),
        "error should explain the subscription restriction, got: {err}"
    );

    // Nothing should have been written.
    let db = harness.db().clone();
    let repo = CredentialRepository::new(db, EventBus::noop());
    assert!(
        repo.get_encrypted_raw("KIMI_API_KEY")
            .await
            .unwrap()
            .is_none(),
        "rejected subscription must not be persisted"
    );
}

#[tokio::test]
async fn credential_set_rejects_subscription_without_user_context() {
    let harness = McpTestHarness::new().await;

    // Even without `org_shared`, a subscription with no resolvable acting user
    // would fall through to an org-shared (owner_user_id = NULL) write — refuse.
    let res = harness
        .call_tool(
            "credential_set",
            json!({
                "provider_id":"kimi-for-coding",
                "key_name":"KIMI_API_KEY",
                "api_key":"sk-personal"
            }),
        )
        .await
        .expect("credential_set should dispatch");

    assert_eq!(res["ok"], false);
    let err = res["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("subscription"),
        "error should explain the subscription restriction, got: {err}"
    );
}

#[tokio::test]
async fn credential_set_allows_org_shared_api_key() {
    let harness = McpTestHarness::new().await;

    // API-key providers retain the org-shared capability.
    let res = harness
        .call_tool(
            "credential_set",
            json!({
                "provider_id":"anthropic",
                "key_name":"ANTHROPIC_API_KEY",
                "api_key":"sk-org",
                "org_shared": true
            }),
        )
        .await
        .expect("credential_set should dispatch");

    assert_eq!(
        res["ok"], true,
        "api-key org-shared write should succeed: {res:?}"
    );
}

#[tokio::test]
async fn credential_delete_removes_credential() {
    let harness = McpTestHarness::new().await;

    let _ = harness
        .call_tool(
            "credential_set",
            json!({"provider_id":"openai","key_name":"OPENAI_API_KEY","api_key":"a"}),
        )
        .await
        .expect("credential_set should dispatch");

    let deleted = harness
        .call_tool("credential_delete", json!({"key_name":"OPENAI_API_KEY"}))
        .await
        .expect("credential_delete should dispatch");
    assert_eq!(deleted["ok"], true);
    assert_eq!(deleted["deleted"], true);
}
