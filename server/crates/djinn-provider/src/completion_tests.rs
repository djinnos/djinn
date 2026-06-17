use std::pin::Pin;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::anyhow;
use futures::{Stream, stream};
use serde_json::Value;

use super::*;
use crate::catalog::builtin::{AuthShape, FormatRule};
use crate::provider::error::ProviderError;
use crate::provider::{
    AuthMethod, FormatFamily, ProviderCapabilities, ReasoningEffort, ToolChoice,
};
use djinn_core::models::Pricing;
use djinn_db::UserRepository;

#[test]
fn production_memory_resolver_does_not_list_all_credentials() {
    let production_sources = [
        (
            "djinn-provider/src/completion.rs",
            include_str!("completion.rs"),
        ),
        (
            "djinn-agent/src/actors/slot/llm_extraction.rs",
            include_str!("../../djinn-agent/src/actors/slot/llm_extraction.rs"),
        ),
        (
            "djinn-control-plane/src/tools/memory_tools/summaries.rs",
            include_str!("../../djinn-control-plane/src/tools/memory_tools/summaries.rs"),
        ),
        (
            "djinn-control-plane/src/tools/memory_tools/write_dedup_runtime.rs",
            include_str!("../../djinn-control-plane/src/tools/memory_tools/write_dedup_runtime.rs"),
        ),
        (
            "djinn-control-plane/src/tools/memory_tools/lifecycle.rs",
            include_str!("../../djinn-control-plane/src/tools/memory_tools/lifecycle.rs"),
        ),
        (
            "djinn-control-plane/src/tools/memory_tools/contradiction.rs",
            include_str!("../../djinn-control-plane/src/tools/memory_tools/contradiction.rs"),
        ),
    ];

    for (path, source) in production_sources {
        let production_segment = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source segment should exist");
        for forbidden in [
            "CredentialRepository::list(",
            ".list().await",
            "credential_repo.list()",
        ] {
            assert!(
                !production_segment.contains(forbidden),
                "production memory provider resolution in {path} must use scoped list_for_user()/scoped credential loaders, not {forbidden}"
            );
        }
    }
}

#[test]
fn transient_error_prefers_typed_then_substring() {
    // Typed retryable variants short-circuit to true.
    assert!(is_transient_error(
        &anyhow::Error::new(ProviderError::RateLimit {
            retry_after_ms: None
        })
        .context("provider API error 429")
    ));
    assert!(is_transient_error(
        &anyhow::Error::new(ProviderError::Transport).context("SSE read error")
    ));
    // Typed terminal variants short-circuit to false even if the message
    // would otherwise match a substring.
    assert!(!is_transient_error(
        &anyhow::Error::new(ProviderError::Authentication)
            .context("provider API error 401: timeout while authing")
    ));
    // Untyped errors fall back to substring matching.
    assert!(is_transient_error(&anyhow!("connection reset by peer")));
    assert!(!is_transient_error(&anyhow!("bad request: missing field")));
}

fn setup_catalog() -> CatalogService {
    let catalog = CatalogService::new();
    catalog.inject_builtin_providers(builtin::BUILTIN_PROVIDERS);
    catalog
}

fn credential(provider_id: &str, key_name: &str) -> Credential {
    credential_with_owner(provider_id, key_name, None)
}

fn credential_with_owner(
    provider_id: &str,
    key_name: &str,
    owner_user_id: Option<&str>,
) -> Credential {
    Credential {
        id: "cred".to_string(),
        provider_id: provider_id.to_string(),
        key_name: key_name.to_string(),
        owner_user_id: owner_user_id.map(ToOwned::to_owned),
        created_at: "now".to_string(),
        updated_at: "now".to_string(),
    }
}

fn ensure_test_vault_key() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let path = std::path::Path::new("/var/tmp/djinn-test-vault/vault.key");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create test vault dir");
        }
        if !path.exists() {
            std::fs::write(path, [7u8; 32]).expect("write test vault key");
        }
    });
}

fn repo() -> CredentialRepository {
    ensure_test_vault_key();
    let db = Database::open_in_memory().expect("test db");
    CredentialRepository::new(db, EventBus::noop())
}

fn test_model(provider_id: &str, id: &str, reasoning: bool) -> Model {
    Model {
        id: id.to_string(),
        provider_id: provider_id.to_string(),
        name: id.to_string(),
        tool_call: true,
        reasoning,
        attachment: false,
        context_window: 128_000,
        output_limit: 64_000,
        pricing: Pricing::default(),
    }
}

#[test]
fn api_key_provider_config_defaults_reasoning_for_anthropic_reasoning_model() {
    let builtin_provider = builtin::find_builtin_provider("minimax-coding-plan")
        .expect("minimax provider row should exist");
    let model = test_model(
        "minimax-coding-plan",
        "minimax-coding-plan/MiniMax-M1",
        true,
    );

    let config = api_key_provider_config(
        "minimax-coding-plan",
        &model,
        builtin_provider,
        "test-key".to_string(),
    );

    assert_eq!(config.format_family, FormatFamily::Anthropic);
    assert_eq!(config.reasoning_effort, Some(ReasoningEffort::Medium));
    assert!(matches!(config.auth, AuthMethod::BearerToken(ref key) if key == "test-key"));
    assert_eq!(config.capabilities.max_tokens_default, Some(64_000));
}

#[test]
fn api_key_provider_config_keeps_non_reasoning_model_disabled() {
    let builtin_provider =
        builtin::find_builtin_provider("anthropic").expect("anthropic provider row should exist");
    let model = test_model("anthropic", "anthropic/claude-3-5-haiku-latest", false);

    let config = api_key_provider_config(
        "anthropic",
        &model,
        builtin_provider,
        "test-key".to_string(),
    );

    assert_eq!(config.format_family, FormatFamily::Anthropic);
    assert_eq!(config.reasoning_effort, None);
}

#[test]
fn api_key_provider_config_preserves_openai_reasoning_policy() {
    let builtin_provider =
        builtin::find_builtin_provider("openai").expect("openai provider row should exist");
    let chat_model = test_model("openai", "gpt-4.1-mini", true);
    let responses_model = test_model("openai", "gpt-5.1", true);

    let chat_config = api_key_provider_config(
        "openai",
        &chat_model,
        builtin_provider,
        "test-key".to_string(),
    );
    let responses_config = api_key_provider_config(
        "openai",
        &responses_model,
        builtin_provider,
        "test-key".to_string(),
    );

    assert_eq!(chat_config.format_family, FormatFamily::OpenAI);
    assert_eq!(chat_config.reasoning_effort, None);
    assert_eq!(
        responses_config.format_family,
        FormatFamily::OpenAIResponses
    );
    assert_eq!(responses_config.reasoning_effort, None);
}

enum ProviderBehavior {
    Stream(Vec<anyhow::Result<StreamEvent>>),
    Error(String),
}

struct MockProvider {
    name: &'static str,
    calls: AtomicUsize,
    behaviors: Mutex<Vec<ProviderBehavior>>,
}

impl MockProvider {
    fn new(behaviors: Vec<ProviderBehavior>) -> Self {
        Self {
            name: "mock",
            calls: AtomicUsize::new(0),
            behaviors: Mutex::new(behaviors),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [Value],
        _tool_choice: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self
            .behaviors
            .lock()
            .expect("mock behaviors lock")
            .remove(0);
        Box::pin(async move {
            match behavior {
                ProviderBehavior::Stream(events) => {
                    let stream: Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                        Box::pin(stream::iter(events));
                    Ok(stream)
                }
                ProviderBehavior::Error(message) => Err(anyhow!(message)),
            }
        })
    }
}

#[test]
fn parses_memory_llm_model_from_settings_raw() {
    let raw = r#"{"memory":{"llm_model":"openai/gpt-4.1-mini"}}"#;
    assert_eq!(
        parse_memory_model_selection(raw).as_deref(),
        Some("openai/gpt-4.1-mini")
    );
}

#[test]
fn fallback_picks_cheapest_connected_builtin_model() {
    let catalog = setup_catalog();
    let credentials = vec![credential("openai", "OPENAI_API_KEY")];

    let resolved = select_memory_model(&catalog, &credentials, None).expect("select model");

    assert_eq!(resolved.effective_provider_id, "openai");
    assert_eq!(resolved.model.provider_id, "openai");
}

#[test]
fn unavailable_model_returns_descriptive_error() {
    let catalog = setup_catalog();
    let credentials = vec![credential("openai", "OPENAI_API_KEY")];

    let error = select_memory_model(&catalog, &credentials, Some("openai/does-not-exist"))
        .expect_err("missing model should error");

    assert!(
        error
            .to_string()
            .contains("memory.llm_model 'openai/does-not-exist' is not available")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_credential_returns_descriptive_error() {
    let catalog = setup_catalog();
    let repo = repo();
    let resolved = select_memory_model(
        &catalog,
        &[credential("openai", "OPENAI_API_KEY")],
        Some("openai/gpt-4.1-mini"),
    )
    .expect("model should exist");

    let error = match provider_config_for_model(&resolved, &repo).await {
        Ok(_) => panic!("missing secret should error"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("missing credential 'OPENAI_API_KEY'")
    );
}

#[test]
fn api_key_config_defaults_reasoning_for_anthropic_reasoning_model() {
    let model = test_model("minimax-coding-plan", "MiniMax-M3", true);
    let config = api_key_provider_config(
        "minimax-coding-plan",
        &model,
        builtin::find_builtin_provider("minimax-coding-plan").expect("minimax builtin"),
        "minimax-secret".to_string(),
    );

    assert_eq!(config.format_family, FormatFamily::Anthropic);
    assert_eq!(config.reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(config.model_id, "MiniMax-M3");
    assert!(matches!(config.auth, AuthMethod::BearerToken(token) if token == "minimax-secret"));
}

#[test]
fn api_key_config_leaves_non_reasoning_model_without_reasoning_effort() {
    let model = test_model("openai", "gpt-4.1-mini", false);
    let config = api_key_provider_config(
        "openai",
        &model,
        builtin::find_builtin_provider("openai").expect("openai builtin"),
        "openai-secret".to_string(),
    );

    assert_eq!(config.format_family, FormatFamily::OpenAI);
    assert_eq!(config.reasoning_effort, None);
    assert!(matches!(config.auth, AuthMethod::BearerToken(token) if token == "openai-secret"));
}

#[test]
fn default_reasoning_policy_preserves_openai_wire_behavior() {
    assert_eq!(
        default_reasoning_effort_for_model(true, FormatFamily::OpenAI, "gpt-4.1-mini"),
        None
    );
    assert_eq!(
        default_reasoning_effort_for_model(true, FormatFamily::OpenAIResponses, "gpt-5.1"),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oauth_provider_config_uses_stored_tokens() {
    let catalog = setup_catalog();
    let repo = repo();
    let tokens = CodexTokens {
        access_token: "access_test".to_string(),
        refresh_token: "refresh_test".to_string(),
        id_token: None,
        expires_at: i64::MAX,
        account_id: None,
    };
    tokens.save_to_db(&repo).await.expect("save oauth tokens");

    let resolved = select_memory_model(
        &catalog,
        &[credential("openai", "__OAUTH_CHATGPT_CODEX")],
        Some("openai/codex-mini-latest"),
    )
    .expect("oauth model should resolve");

    let config = provider_config_for_model(&resolved, &repo)
        .await
        .expect("oauth config should resolve");

    assert_eq!(config.model_id, resolved.model.id);
    assert!(matches!(config.auth, AuthMethod::BearerToken(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_provider_config_honors_explicit_user_scope() {
    ensure_test_vault_key();
    let catalog = setup_catalog();
    let db = Database::open_in_memory().expect("test db");
    let users = UserRepository::new(db.clone());
    let alice = users
        .upsert_from_github(7001, "memory-config-alice", None, None)
        .await
        .expect("seed alice")
        .id;
    let bob = users
        .upsert_from_github(7002, "memory-config-bob", None, None)
        .await
        .expect("seed bob")
        .id;
    let repo = CredentialRepository::new(db, EventBus::noop());
    repo.set_with_owner("openai", "OPENAI_API_KEY", "bob-secret", Some(&bob))
        .await
        .expect("save bob key");

    let resolved = select_memory_model(
        &catalog,
        &[credential_with_owner(
            "openai",
            "OPENAI_API_KEY",
            Some(&bob),
        )],
        Some("openai/gpt-4.1-mini"),
    )
    .expect("model should resolve from supplied listing");

    let error = match provider_config_for_model_for_user(&resolved, &repo, Some(&alice)).await {
        Ok(_) => panic!("alice must not read bob's key"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("missing credential 'OPENAI_API_KEY'")
    );

    repo.set_with_owner("openai", "OPENAI_API_KEY", "alice-secret", Some(&alice))
        .await
        .expect("save alice key");
    let config = provider_config_for_model_for_user(&resolved, &repo, Some(&alice))
        .await
        .expect("alice key should resolve");
    match config.auth {
        AuthMethod::BearerToken(token) => assert_eq!(token, "alice-secret"),
        _ => panic!("expected bearer auth"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_provider_config_honors_explicit_user_scope() {
    ensure_test_vault_key();
    let catalog = setup_catalog();
    let db = Database::open_in_memory().expect("test db");
    let users = UserRepository::new(db.clone());
    let alice = users
        .upsert_from_github(7003, "memory-config-codex-alice", None, None)
        .await
        .expect("seed alice")
        .id;
    let bob = users
        .upsert_from_github(7004, "memory-config-codex-bob", None, None)
        .await
        .expect("seed bob")
        .id;
    let repo = CredentialRepository::new(db, EventBus::noop());
    let bob_tokens = CodexTokens {
        access_token: "bob-access".to_string(),
        refresh_token: "bob-refresh".to_string(),
        id_token: None,
        expires_at: i64::MAX,
        account_id: None,
    };
    repo.set_with_owner(
        "chatgpt_codex",
        "__OAUTH_CHATGPT_CODEX",
        &serde_json::to_string(&bob_tokens).expect("serialize bob tokens"),
        Some(&bob),
    )
    .await
    .expect("save bob tokens");

    let resolved = select_memory_model(
        &catalog,
        &[credential_with_owner(
            "chatgpt_codex",
            "__OAUTH_CHATGPT_CODEX",
            Some(&bob),
        )],
        Some("openai/codex-mini-latest"),
    )
    .expect("oauth model should resolve from supplied listing");

    let error = match provider_config_for_model_for_user(&resolved, &repo, Some(&alice)).await {
        Ok(_) => panic!("alice must not read bob's tokens"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("missing OAuth tokens"));

    let alice_tokens = CodexTokens {
        access_token: "alice-access".to_string(),
        refresh_token: "alice-refresh".to_string(),
        id_token: None,
        expires_at: i64::MAX,
        account_id: None,
    };
    repo.set_with_owner(
        "chatgpt_codex",
        "__OAUTH_CHATGPT_CODEX",
        &serde_json::to_string(&alice_tokens).expect("serialize alice tokens"),
        Some(&alice),
    )
    .await
    .expect("save alice tokens");

    let config = provider_config_for_model_for_user(&resolved, &repo, Some(&alice))
        .await
        .expect("alice tokens should resolve");
    match config.auth {
        AuthMethod::BearerToken(token) => assert_eq!(token, "alice-access"),
        _ => panic!("expected bearer auth"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_collects_text() {
    let provider = MockProvider::new(vec![ProviderBehavior::Stream(vec![
        Ok(StreamEvent::Delta(ContentBlock::text("hello "))),
        Ok(StreamEvent::Delta(ContentBlock::text("world"))),
        Ok(StreamEvent::Done),
    ])]);

    let response = complete(
        &provider,
        CompletionRequest {
            system: "system".into(),
            prompt: "prompt".into(),
            max_tokens: 12,
        },
    )
    .await
    .unwrap();

    assert_eq!(response.text, "hello world");
    assert_eq!(response.input_tokens, 0);
    assert_eq!(response.output_tokens, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_propagates_errors() {
    let provider = MockProvider::new(vec![ProviderBehavior::Error("boom".into())]);

    let error = complete(
        &provider,
        CompletionRequest {
            system: "system".into(),
            prompt: "prompt".into(),
            max_tokens: 12,
        },
    )
    .await
    .expect_err("expected completion to fail");

    assert!(
        error
            .to_string()
            .contains("provider stream initialization failed")
    );
    assert_eq!(provider.call_count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_collects_usage() {
    let provider = MockProvider::new(vec![ProviderBehavior::Stream(vec![
        Ok(StreamEvent::Usage(TokenUsage {
            input: 11,
            output: 7,
            ..Default::default()
        })),
        Ok(StreamEvent::Delta(ContentBlock::text("ok"))),
        Ok(StreamEvent::Done),
    ])]);

    let response = complete(
        &provider,
        CompletionRequest {
            system: "system".into(),
            prompt: "prompt".into(),
            max_tokens: 12,
        },
    )
    .await
    .unwrap();

    assert_eq!(response.text, "ok");
    assert_eq!(response.input_tokens, 11);
    assert_eq!(response.output_tokens, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_retries_transient_error_once() {
    let provider = MockProvider::new(vec![
        ProviderBehavior::Error("429 rate limit".into()),
        ProviderBehavior::Stream(vec![
            Ok(StreamEvent::Delta(ContentBlock::text("retry ok"))),
            Ok(StreamEvent::Done),
        ]),
    ]);

    let response = complete(
        &provider,
        CompletionRequest {
            system: "system".into(),
            prompt: "prompt".into(),
            max_tokens: 12,
        },
    )
    .await
    .unwrap();

    assert_eq!(response.text, "retry ok");
    assert_eq!(provider.call_count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_uses_configured_model() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    settings
        .set(
            "settings.raw",
            r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
        )
        .await
        .unwrap();
    credentials
        .set("anthropic", "ANTHROPIC_API_KEY", "test-key")
        .await
        .unwrap();

    let provider = resolve_memory_provider(&db).await.unwrap();
    assert_eq!(provider.name(), "anthropic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_falls_back_to_settings_model_priority() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    // Configure a model in settings models list (what the UI does).
    settings
        .set("settings.raw", r#"{"models":["openai/gpt-4.1-mini"]}"#)
        .await
        .unwrap();
    credentials
        .set("openai", "OPENAI_API_KEY", "test-key")
        .await
        .unwrap();

    let provider = resolve_memory_provider(&db).await.unwrap();
    assert_eq!(provider.name(), "openai");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_prefers_user_settings_before_global_settings() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    let user = UserRepository::new(db.clone())
        .upsert_from_github(1003, "user-c", None, None)
        .await
        .unwrap();
    settings
        .set(
            "settings.raw",
            r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
        )
        .await
        .unwrap();
    UserSettingsRepository::new(db.clone())
        .upsert_models(&user.id, &["openai/gpt-4.1-mini".to_string()])
        .await
        .unwrap();
    credentials
        .set_with_owner("openai", "OPENAI_API_KEY", "caller-key", Some(&user.id))
        .await
        .unwrap();
    credentials
        .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "org-key", None)
        .await
        .unwrap();

    let provider = resolve_memory_provider_for_user(&db, Some(&user.id))
        .await
        .expect("caller user settings should outrank global settings");

    assert_eq!(provider.name(), "openai");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_filters_stale_user_selection_to_visible_credentials() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    let user = UserRepository::new(db.clone())
        .upsert_from_github(1004, "user-d", None, None)
        .await
        .unwrap();
    let other_user = UserRepository::new(db.clone())
        .upsert_from_github(1005, "user-e", None, None)
        .await
        .unwrap();
    settings
        .set(
            "settings.raw",
            r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
        )
        .await
        .unwrap();
    UserSettingsRepository::new(db.clone())
        .upsert_models(&user.id, &["openai/gpt-4.1-mini".to_string()])
        .await
        .unwrap();
    credentials
        .set_with_owner(
            "openai",
            "OPENAI_API_KEY",
            "other-private-key",
            Some(&other_user.id),
        )
        .await
        .unwrap();
    credentials
        .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "org-key", None)
        .await
        .unwrap();

    let provider = resolve_memory_provider_for_user(&db, Some(&user.id))
        .await
        .expect("stale hidden user model should fall through to visible global model");

    assert_eq!(provider.name(), "anthropic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_for_user_sees_private_and_org_shared_credentials() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    let user = UserRepository::new(db.clone())
        .upsert_from_github(1001, "user-a", None, None)
        .await
        .unwrap();
    settings
        .set("settings.raw", r#"{"models":["openai/gpt-4.1-mini"]}"#)
        .await
        .unwrap();
    credentials
        .set_with_owner("openai", "OPENAI_API_KEY", "caller-key", Some(&user.id))
        .await
        .unwrap();
    credentials
        .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "org-key", None)
        .await
        .unwrap();

    let caller_provider = resolve_memory_provider_for_user(&db, Some(&user.id))
        .await
        .expect("caller should resolve their private configured provider");
    assert_eq!(caller_provider.name(), "openai");

    settings
        .set(
            "settings.raw",
            r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
        )
        .await
        .unwrap();
    let fallback_provider = resolve_memory_provider_for_user(&db, Some(&user.id))
        .await
        .expect("caller should resolve org-shared fallback provider");
    assert_eq!(fallback_provider.name(), "anthropic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_background_ignores_another_users_private_credential() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    let other_user = UserRepository::new(db.clone())
        .upsert_from_github(1002, "user-b", None, None)
        .await
        .unwrap();
    settings
        .set("settings.raw", r#"{"models":["openai/gpt-4.1-mini"]}"#)
        .await
        .unwrap();
    credentials
        .set_with_owner(
            "openai",
            "OPENAI_API_KEY",
            "user-b-key",
            Some(&other_user.id),
        )
        .await
        .unwrap();
    credentials
        .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "org-key", None)
        .await
        .unwrap();

    let provider = resolve_memory_provider_for_user(&db, None)
        .await
        .expect("background scope should fall back to org-shared credentials only");

    assert_eq!(provider.name(), "anthropic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_background_ignores_user_settings_and_uses_org_shared_only() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    let user = UserRepository::new(db.clone())
        .upsert_from_github(1006, "user-f", None, None)
        .await
        .unwrap();
    settings
        .set(
            "settings.raw",
            r#"{"models":["anthropic/claude-3-5-haiku-latest"]}"#,
        )
        .await
        .unwrap();
    UserSettingsRepository::new(db.clone())
        .upsert_models(&user.id, &["openai/gpt-4.1-mini".to_string()])
        .await
        .unwrap();
    credentials
        .set_with_owner("openai", "OPENAI_API_KEY", "private-key", Some(&user.id))
        .await
        .unwrap();
    credentials
        .set_with_owner("anthropic", "ANTHROPIC_API_KEY", "org-key", None)
        .await
        .unwrap();

    let provider = resolve_memory_provider_for_user(&db, None)
        .await
        .expect("background scope should use only org-shared global candidates");

    assert_eq!(provider.name(), "anthropic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_errors_when_unavailable() {
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    settings
        .set("settings.raw", r#"{"models":["openai/nonexistent-model"]}"#)
        .await
        .unwrap();

    let error = match resolve_memory_provider(&db).await {
        Ok(_) => panic!("expected memory provider resolution to fail"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("no connected builtin provider models are available")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_memory_provider_falls_back_to_connected_when_configured_model_unresolvable() {
    // Staging scenario: the configured (first worker) model isn't resolvable
    // against the static catalog — e.g. a codex-served `openai/*` id that
    // dispatch runs but `find_model` doesn't know — yet another provider IS
    // connected. Extraction must still run on the connected model instead of
    // being skipped, which is what left the deployment with zero scoped notes.
    let db = Database::open_in_memory().unwrap();
    let settings = SettingsRepository::new(db.clone(), EventBus::noop());
    let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
    settings
        .set(
            "settings.raw",
            r#"{"models":["openai/gpt-5.5-not-in-catalog"]}"#,
        )
        .await
        .unwrap();
    // A genuinely connected builtin provider for the fallback to land on.
    credentials
        .set("anthropic", "ANTHROPIC_API_KEY", "test-key")
        .await
        .unwrap();

    let provider = resolve_memory_provider(&db)
        .await
        .expect("should fall back to the connected provider, not error");
    assert_eq!(provider.name(), "anthropic");
}

/// G8 lock-in: every builtin provider row must resolve to exactly the
/// (format_family, capabilities, auth shape) that the old per-provider
/// `match` arms produced. This is a golden table — changing any value here
/// is a behavior change and must be intentional.
#[test]
fn builtin_rows_lock_format_capabilities_and_auth() {
    // (id, expected fixed format family OR None for openai's model-dependent
    // rule, auth shape, streaming, max_tokens_default)
    struct Expected {
        id: &'static str,
        // None == OpenAI's model-dependent `OpenAiResponsesByModel` rule.
        fixed_family: Option<FormatFamily>,
        auth_shape: AuthShape,
        streaming: bool,
        max_tokens_default: Option<u32>,
    }

    let expected = [
        Expected {
            id: "anthropic",
            fixed_family: Some(FormatFamily::Anthropic),
            auth_shape: AuthShape::Header("x-api-key"),
            streaming: true,
            max_tokens_default: Some(64_000),
        },
        Expected {
            id: "openai",
            fixed_family: None, // OpenAiResponsesByModel
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "google",
            fixed_family: Some(FormatFamily::Google),
            auth_shape: AuthShape::Header("x-goog-api-key"),
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "fireworks-ai",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "minimax-coding-plan",
            fixed_family: Some(FormatFamily::Anthropic),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: Some(64_000),
        },
        Expected {
            id: "xiaomi-mimo",
            fixed_family: Some(FormatFamily::Anthropic),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: Some(64_000),
        },
        Expected {
            id: "kimi-coding-plan",
            fixed_family: Some(FormatFamily::Anthropic),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: Some(64_000),
        },
        Expected {
            id: "opencode",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "zai-coding-plan",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "chatgpt_codex",
            fixed_family: Some(FormatFamily::OpenAIResponses),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "githubcopilot",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "github_app",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "gcp_vertex_ai",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "aws_bedrock",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
        Expected {
            id: "azure_openai",
            fixed_family: Some(FormatFamily::OpenAI),
            auth_shape: AuthShape::Bearer,
            streaming: true,
            max_tokens_default: None,
        },
    ];

    // The expected table must cover every builtin row, no more, no less.
    assert_eq!(
        expected.len(),
        builtin::BUILTIN_PROVIDERS.len(),
        "expected table is out of sync with BUILTIN_PROVIDERS — add the new provider's golden mapping"
    );

    // A non-responses and a responses OpenAI model id to exercise the
    // model-dependent rule.
    const PLAIN_MODEL: &str = "gpt-4.1-mini";
    const RESPONSES_MODEL: &str = "gpt-5.1";
    assert!(!builtin::is_openai_responses_model(PLAIN_MODEL));
    assert!(builtin::is_openai_responses_model(RESPONSES_MODEL));

    for exp in &expected {
        let provider = builtin::find_builtin_provider(exp.id)
            .unwrap_or_else(|| panic!("builtin provider '{}' not found", exp.id));

        // Auth shape.
        assert_eq!(
            provider.auth_shape, exp.auth_shape,
            "auth shape mismatch for '{}'",
            exp.id
        );
        // And the concrete AuthMethod the shape produces.
        match exp.auth_shape {
            AuthShape::Bearer => assert!(
                matches!(
                    provider.auth_method("k".to_string()),
                    AuthMethod::BearerToken(ref t) if t == "k"
                ),
                "auth method mismatch for '{}'",
                exp.id
            ),
            AuthShape::Header(name) => assert!(
                matches!(
                    provider.auth_method("k".to_string()),
                    AuthMethod::ApiKeyHeader { ref header, ref key }
                        if header == name && key == "k"
                ),
                "auth method mismatch for '{}'",
                exp.id
            ),
        }

        // Capabilities.
        let caps: ProviderCapabilities = provider.capabilities();
        assert_eq!(
            caps.streaming, exp.streaming,
            "streaming mismatch for '{}'",
            exp.id
        );
        assert_eq!(
            caps.max_tokens_default, exp.max_tokens_default,
            "max_tokens_default mismatch for '{}'",
            exp.id
        );

        // Format family.
        match exp.fixed_family {
            Some(family) => {
                assert_eq!(
                    provider.format_rule,
                    FormatRule::Fixed(family),
                    "format rule mismatch for '{}'",
                    exp.id
                );
                // Fixed families ignore the model id.
                assert_eq!(
                    provider.format_family(PLAIN_MODEL),
                    family,
                    "format family (plain) mismatch for '{}'",
                    exp.id
                );
                assert_eq!(
                    provider.format_family(RESPONSES_MODEL),
                    family,
                    "format family (responses) mismatch for '{}'",
                    exp.id
                );
            }
            None => {
                // openai: model-dependent OpenAI / OpenAIResponses.
                assert_eq!(
                    provider.format_rule,
                    FormatRule::OpenAiResponsesByModel,
                    "format rule mismatch for '{}'",
                    exp.id
                );
                assert_eq!(
                    provider.format_family(PLAIN_MODEL),
                    FormatFamily::OpenAI,
                    "openai non-responses model should be OpenAI"
                );
                assert_eq!(
                    provider.format_family(RESPONSES_MODEL),
                    FormatFamily::OpenAIResponses,
                    "openai responses model should be OpenAIResponses"
                );
            }
        }
    }
}
