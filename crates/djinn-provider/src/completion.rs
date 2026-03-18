use anyhow::{Context, Result, anyhow};
use djinn_core::events::EventBus;
use djinn_core::message::{ContentBlock, Conversation, Message};
use djinn_core::models::{Credential, Model};
use djinn_db::{Database, SettingsRepository};
use futures::StreamExt;
use tokio::time::{Duration, timeout};

use crate::catalog::{CatalogService, builtin};
use crate::oauth::{self, codex::CodexTokens, copilot::CopilotTokens};
use crate::provider::{
    AuthMethod, FormatFamily, LlmProvider, ProviderCapabilities, ProviderConfig, StreamEvent,
    TokenUsage, create_provider,
};
use crate::repos::CredentialRepository;

const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const MEMORY_MODEL_SETTING_KEY: &str = "memory.llm_model";

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: String,
    pub prompt: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletionResponse {
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct MemoryModelSelection {
    pub(crate) selected_model_id: Option<String>,
}

impl MemoryModelSelection {
    pub(crate) fn from_settings_raw(raw: &str) -> Self {
        let value = match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(value) => value,
            Err(_) => return Self::default(),
        };

        let selected_model_id = value
            .get("memory")
            .and_then(|memory| memory.get("llm_model"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);

        Self { selected_model_id }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedMemoryModel {
    pub(crate) model: Model,
    pub(crate) effective_provider_id: String,
}

pub(crate) fn parse_memory_model_selection(raw: &str) -> Option<String> {
    MemoryModelSelection::from_settings_raw(raw).selected_model_id
}

pub(crate) fn select_memory_model(
    catalog: &CatalogService,
    credentials: &[Credential],
    selected_model_id: Option<&str>,
) -> Result<ResolvedMemoryModel> {
    let connected = catalog.connected_provider_ids(credentials);

    if let Some(model_id) = selected_model_id.map(str::trim).filter(|value| !value.is_empty()) {
        let model = catalog.find_model(model_id).ok_or_else(|| {
            anyhow!(
                "memory.llm_model '{}' is not available in the provider catalog",
                model_id
            )
        })?;
        let effective_provider_id = effective_provider_for_model(&model, credentials)?;
        return Ok(ResolvedMemoryModel {
            model,
            effective_provider_id,
        });
    }

    let mut candidates = builtin::BUILTIN_PROVIDERS
        .iter()
        .flat_map(|provider| catalog.list_models(provider.id).into_iter())
        .filter(|model| connected.contains(&model.provider_id))
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        total_price(left)
            .partial_cmp(&total_price(right))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.provider_id.cmp(&right.provider_id))
            .then_with(|| left.id.cmp(&right.id))
    });

    let model = candidates.into_iter().next().ok_or_else(|| {
        anyhow!(
            "no connected builtin provider models are available for memory.llm_model fallback"
        )
    })?;
    let effective_provider_id = effective_provider_for_model(&model, credentials)?;
    Ok(ResolvedMemoryModel {
        model,
        effective_provider_id,
    })
}

pub async fn complete(
    provider: &dyn LlmProvider,
    request: CompletionRequest,
) -> Result<CompletionResponse> {
    let mut attempt = 0;
    loop {
        let conversation = build_conversation(&request);
        match timeout(
            COMPLETION_TIMEOUT,
            collect_completion(provider, conversation),
        )
        .await
        {
            Ok(Ok(response)) => return Ok(response),
            Ok(Err(error)) if attempt == 0 && is_transient_error(&error) => {
                attempt += 1;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) if attempt == 0 => {
                let error = anyhow!("completion timed out after {}s", COMPLETION_TIMEOUT.as_secs());
                if is_transient_error(&error) {
                    attempt += 1;
                } else {
                    return Err(error);
                }
            }
            Err(_) => {
                return Err(anyhow!(
                    "completion timed out after {}s",
                    COMPLETION_TIMEOUT.as_secs()
                ));
            }
        }
    }
}

async fn collect_completion(
    provider: &dyn LlmProvider,
    conversation: Conversation,
) -> Result<CompletionResponse> {
    let stream = provider
        .stream(&conversation, &[], None)
        .await
        .context("provider stream initialization failed")?;

    tokio::pin!(stream);

    let mut text = String::new();
    let mut usage = TokenUsage::default();

    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::Delta(ContentBlock::Text { text: delta }) => text.push_str(&delta),
            StreamEvent::Usage(token_usage) => usage = token_usage,
            StreamEvent::Done => break,
            StreamEvent::Delta(_) => {}
        }
    }

    Ok(CompletionResponse {
        text,
        input_tokens: usage.input,
        output_tokens: usage.output,
    })
}

fn build_conversation(request: &CompletionRequest) -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::system(request.system.clone()));
    conversation.push(Message::user(request.prompt.clone()));
    conversation
}

pub async fn resolve_memory_provider(db: &Database) -> Result<Box<dyn LlmProvider>> {
    let event_bus = EventBus::noop();
    let settings_repo = SettingsRepository::new(db.clone(), event_bus.clone());
    let configured_model = settings_repo
        .get(MEMORY_MODEL_SETTING_KEY)
        .await?
        .map(|setting| setting.value)
        .filter(|value| !value.trim().is_empty());

    let settings_raw = match configured_model {
        Some(model_id) => format!(r#"{{"memory":{{"llm_model":"{}"}}}}"#, model_id),
        None => "{}".to_string(),
    };

    let catalog = CatalogService::new();
    catalog.inject_builtin_providers(builtin::BUILTIN_PROVIDERS);

    let credential_repo = CredentialRepository::new(db.clone(), event_bus);
    let credentials = credential_repo.list().await?;
    let provider_config = resolve_memory_provider_config(
        &catalog,
        &credentials,
        &credential_repo,
        &settings_raw,
    )
    .await?;

    Ok(create_provider(provider_config))
}

pub async fn resolve_memory_provider_config(
    catalog: &CatalogService,
    credentials: &[Credential],
    credential_repo: &CredentialRepository,
    settings_raw: &str,
) -> Result<ProviderConfig> {
    let selected = parse_memory_model_selection(settings_raw);
    let resolved = select_memory_model(catalog, credentials, selected.as_deref())?;
    provider_config_for_model(&resolved, credential_repo).await
}

pub(crate) async fn provider_config_for_model(
    resolved: &ResolvedMemoryModel,
    credential_repo: &CredentialRepository,
) -> Result<ProviderConfig> {
    match resolved.effective_provider_id.as_str() {
        "chatgpt_codex" => {
            let tokens = CodexTokens::load_from_db(credential_repo).await.ok_or_else(|| {
                anyhow!(
                    "provider '{}' for memory model '{}' is missing OAuth tokens",
                    resolved.effective_provider_id,
                    resolved.model.id
                )
            })?;
            Ok(provider_config_with_model(
                oauth::codex_provider_config(&tokens),
                &resolved.model,
            ))
        }
        "githubcopilot" => {
            let tokens = CopilotTokens::load_from_db(credential_repo).await.ok_or_else(|| {
                anyhow!(
                    "provider '{}' for memory model '{}' is missing OAuth tokens",
                    resolved.effective_provider_id,
                    resolved.model.id
                )
            })?;
            Ok(provider_config_with_model(
                oauth::copilot_provider_config(&tokens),
                &resolved.model,
            ))
        }
        provider_id => api_key_provider_config(provider_id, &resolved.model, credential_repo).await,
    }
}

fn total_price(model: &Model) -> f64 {
    model.pricing.input_per_million + model.pricing.output_per_million
}

fn effective_provider_for_model(model: &Model, credentials: &[Credential]) -> Result<String> {
    let oauth_provider = builtin::resolve_oauth_provider(&model.provider_id);
    let credential_key_names = credentials
        .iter()
        .map(|credential| credential.key_name.clone())
        .collect::<std::collections::HashSet<_>>();

    if let Some(provider_id) = oauth_provider {
        let oauth_keys = builtin::oauth_keys_for_provider(provider_id);
        if builtin::is_oauth_key_present(&oauth_keys, &credential_key_names) {
            return Ok(provider_id.to_string());
        }
    }

    let builtin_provider = builtin::find_builtin_provider(&model.provider_id).ok_or_else(|| {
        anyhow!(
            "provider '{}' for memory model '{}' is not supported by djinn-provider",
            model.provider_id,
            model.id
        )
    })?;

    if builtin_provider.required_env_vars.is_empty() {
        return Err(anyhow!(
            "provider '{}' for memory model '{}' is unavailable because no OAuth credentials are connected",
            model.provider_id,
            model.id
        ));
    }

    if let Some(key_name) = builtin_provider.required_env_vars.first() {
        if credentials.iter().any(|credential| {
            credential.provider_id == builtin_provider.id && credential.key_name == *key_name
        }) {
            return Ok(builtin_provider.id.to_string());
        }

        return Err(anyhow!(
            "provider '{}' for memory model '{}' is missing credential '{}'",
            builtin_provider.id,
            model.id,
            key_name
        ));
    }

    Err(anyhow!(
        "provider '{}' for memory model '{}' has no supported authentication path",
        builtin_provider.id,
        model.id
    ))
}

fn provider_config_with_model(mut config: ProviderConfig, model: &Model) -> ProviderConfig {
    config.model_id = model.id.clone();
    config.context_window = model.context_window.max(0) as u32;
    config
}

async fn api_key_provider_config(
    provider_id: &str,
    model: &Model,
    credential_repo: &CredentialRepository,
) -> Result<ProviderConfig> {
    let builtin_provider = builtin::find_builtin_provider(provider_id)
        .ok_or_else(|| anyhow!("provider '{}' is not supported by djinn-provider", provider_id))?;
    let key_name = builtin_provider.required_env_vars.first().ok_or_else(|| {
        anyhow!(
            "provider '{}' for memory model '{}' does not support API-key auth",
            provider_id,
            model.id
        )
    })?;
    let api_key = credential_repo.get_decrypted(key_name).await?.ok_or_else(|| {
        anyhow!(
            "provider '{}' for memory model '{}' is missing credential '{}'",
            provider_id,
            model.id,
            key_name
        )
    })?;

    Ok(ProviderConfig {
        base_url: provider_base_url(provider_id),
        auth: provider_auth(provider_id, api_key),
        format_family: provider_format_family(provider_id),
        model_id: model.id.clone(),
        context_window: model.context_window.max(0) as u32,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: provider_capabilities(provider_id),
    })
}

fn provider_base_url(provider_id: &str) -> String {
    match provider_id {
        "anthropic" => "https://api.anthropic.com".to_string(),
        "openai" => "https://api.openai.com".to_string(),
        "google" => "https://generativelanguage.googleapis.com".to_string(),
        _ => "https://api.openai.com".to_string(),
    }
}

fn provider_auth(provider_id: &str, api_key: String) -> AuthMethod {
    match provider_id {
        "anthropic" => AuthMethod::ApiKeyHeader {
            header: "x-api-key".to_string(),
            key: api_key,
        },
        "google" => AuthMethod::ApiKeyHeader {
            header: "x-goog-api-key".to_string(),
            key: api_key,
        },
        _ => AuthMethod::BearerToken(api_key),
    }
}

fn provider_format_family(provider_id: &str) -> FormatFamily {
    match provider_id {
        "anthropic" => FormatFamily::Anthropic,
        "google" => FormatFamily::Google,
        "chatgpt_codex" => FormatFamily::OpenAIResponses,
        _ => FormatFamily::OpenAI,
    }
}

fn provider_capabilities(provider_id: &str) -> ProviderCapabilities {
    match provider_id {
        "anthropic" => ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(8192),
        },
        _ => ProviderCapabilities::default(),
    }
}

fn is_transient_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string().to_ascii_lowercase();
        message.contains("429")
            || message.contains("too many requests")
            || message.contains("connection reset")
            || message.contains("connection refused")
            || message.contains("timed out")
            || message.contains("timeout")
    })
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use anyhow::anyhow;
    use futures::{Stream, stream};
    use serde_json::Value;

    use super::*;
    use crate::provider::ToolChoice;

    fn setup_catalog() -> CatalogService {
        let catalog = CatalogService::new();
        catalog.inject_builtin_providers(builtin::BUILTIN_PROVIDERS);
        catalog
    }

    fn credential(provider_id: &str, key_name: &str) -> Credential {
        Credential {
            id: "cred".to_string(),
            provider_id: provider_id.to_string(),
            key_name: key_name.to_string(),
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        }
    }

    fn repo() -> CredentialRepository {
        let db = Database::open_in_memory().expect("test db");
        CredentialRepository::new(db, EventBus::noop())
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
            let behavior = self.behaviors.lock().expect("mock behaviors lock").remove(0);
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

        assert!(error.to_string().contains("missing credential 'OPENAI_API_KEY'"));
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

        assert!(error.to_string().contains("provider stream initialization failed"));
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn complete_collects_usage() {
        let provider = MockProvider::new(vec![ProviderBehavior::Stream(vec![
            Ok(StreamEvent::Usage(TokenUsage { input: 11, output: 7 })),
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
            .set(MEMORY_MODEL_SETTING_KEY, "anthropic/claude-3-5-haiku-latest")
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
    async fn resolve_memory_provider_falls_back_to_cheapest_available() {
        let db = Database::open_in_memory().unwrap();
        let credentials = CredentialRepository::new(db.clone(), EventBus::noop());
        credentials
            .set("openai", "OPENAI_API_KEY", "test-key")
            .await
            .unwrap();

        let provider = resolve_memory_provider(&db).await.unwrap();
        assert!(!provider.name().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_memory_provider_errors_when_unavailable() {
        let db = Database::open_in_memory().unwrap();
        let settings = SettingsRepository::new(db.clone(), EventBus::noop());
        settings
            .set(MEMORY_MODEL_SETTING_KEY, "openai/nonexistent-model")
            .await
            .unwrap();

        let error = match resolve_memory_provider(&db).await {
            Ok(_) => panic!("expected memory provider resolution to fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("memory.llm_model 'openai/nonexistent-model' is not available")
        );
    }
}
