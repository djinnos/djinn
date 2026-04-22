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
