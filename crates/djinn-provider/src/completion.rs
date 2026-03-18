use anyhow::{Context, Result, anyhow};
use djinn_core::events::EventBus;
use djinn_core::message::{ContentBlock, Conversation, Message};
use djinn_core::models::Model;
use djinn_db::{Database, SettingsRepository};
use futures::StreamExt;
use tokio::time::{Duration, timeout};

use crate::catalog::{CatalogService, builtin};
use crate::oauth::{codex, codex_provider_config, copilot, copilot_provider_config};
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

    let catalog = CatalogService::new();
    catalog.inject_builtin_providers(builtin::BUILTIN_PROVIDERS);

    let credential_repo = CredentialRepository::new(db.clone(), event_bus);
    let provider_config = if let Some(model_id) = configured_model {
        resolve_provider_config_for_model(&catalog, &credential_repo, &model_id).await?
    } else {
        resolve_cheapest_available_provider(&catalog, &credential_repo).await?
    };

    Ok(create_provider(provider_config))
}

async fn resolve_provider_config_for_model(
    catalog: &CatalogService,
    credential_repo: &CredentialRepository,
    model_id: &str,
) -> Result<ProviderConfig> {
    let (provider_id, requested_model_name) = parse_model_id(model_id)?;
    let provider = catalog
        .list_providers()
        .into_iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| anyhow!("provider unavailable for model '{model_id}'"))?;

    let resolved_model = catalog
        .list_models(&provider_id)
        .into_iter()
        .find(|model| model_matches(model, &requested_model_name))
        .ok_or_else(|| anyhow!("model unavailable: {model_id}"))?;

    provider_config_from_parts(catalog, credential_repo, &provider.id, &resolved_model).await
}

async fn resolve_cheapest_available_provider(
    catalog: &CatalogService,
    credential_repo: &CredentialRepository,
) -> Result<ProviderConfig> {
    let mut models: Vec<Model> = catalog
        .list_providers()
        .into_iter()
        .flat_map(|provider| catalog.list_models(&provider.id))
        .collect();

    models.sort_by(|left, right| total_price(left).total_cmp(&total_price(right)));

    let mut last_error = None;
    for model in models {
        match provider_config_from_parts(catalog, credential_repo, &model.provider_id, &model).await {
            Ok(config) => return Ok(config),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no available provider/model found")))
}

async fn provider_config_from_parts(
    catalog: &CatalogService,
    credential_repo: &CredentialRepository,
    provider_id: &str,
    model: &Model,
) -> Result<ProviderConfig> {
    if let Some(config) = load_oauth_provider_config(provider_id, credential_repo).await? {
        return Ok(ProviderConfig {
            model_id: model.id.clone(),
            context_window: model.context_window.max(0) as u32,
            ..config
        });
    }

    let provider = catalog
        .list_providers()
        .into_iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| anyhow!("provider unavailable: {provider_id}"))?;

    let key_name = provider
        .env_vars
        .into_iter()
        .next()
        .unwrap_or_else(|| format!("{}_API_KEY", provider_id.to_ascii_uppercase()));

    let api_key = credential_repo
        .get_decrypted(&key_name)
        .await
        .map_err(|error| anyhow!("credential lookup failed: {error}"))?
        .ok_or_else(|| anyhow!("missing credentials for provider {provider_id} (expected key {key_name})"))?;

    Ok(ProviderConfig {
        base_url: provider
            .base_url
            .clone()
            .if_empty_then(|| default_base_url(provider_id)),
        auth: auth_method_for_provider(provider_id, &api_key),
        format_family: format_family_for_provider(provider_id, &model.id),
        model_id: model.id.clone(),
        context_window: model.context_window.max(0) as u32,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: capabilities_for_provider(provider_id),
    })
}

async fn load_oauth_provider_config(
    provider_id: &str,
    credential_repo: &CredentialRepository,
) -> Result<Option<ProviderConfig>> {
    let effective_oauth_id = match provider_id {
        "chatgpt_codex" | "githubcopilot" => provider_id,
        other => builtin::resolve_oauth_provider(other).unwrap_or(other),
    };

    match effective_oauth_id {
        "chatgpt_codex" => {
            if let Some(tokens) = codex::CodexTokens::load_from_db(credential_repo).await {
                let refreshed = if tokens.is_expired() {
                    codex::refresh_cached_token(&tokens, credential_repo).await?
                } else {
                    tokens
                };
                return Ok(Some(codex_provider_config(&refreshed)));
            }
        }
        "githubcopilot" => {
            if let Some(tokens) = copilot::CopilotTokens::load_from_db(credential_repo).await {
                let refreshed = if tokens.is_expired() {
                    copilot::refresh_copilot_token(&tokens, credential_repo).await?
                } else {
                    tokens
                };
                return Ok(Some(copilot_provider_config(&refreshed)));
            }
        }
        _ => {}
    }

    Ok(None)
}

fn parse_model_id(model_id: &str) -> Result<(String, String)> {
    let Some((provider_id, model_name)) = model_id.split_once('/') else {
        return Err(anyhow!(
            "invalid model id '{model_id}', expected provider/model"
        ));
    };
    Ok((provider_id.to_owned(), model_name.to_owned()))
}

fn model_matches(model: &Model, requested: &str) -> bool {
    let bare = model.id.rsplit('/').next().unwrap_or(&model.id);
    model.id == requested || model.name == requested || bare == requested
}

fn total_price(model: &Model) -> f64 {
    model.pricing.input_per_million + model.pricing.output_per_million
}

fn format_family_for_provider(provider_id: &str, model_id: &str) -> FormatFamily {
    let lower = provider_id.to_lowercase();
    if lower.contains("anthropic") {
        FormatFamily::Anthropic
    } else if lower.contains("google") || lower.contains("gemini") || lower.contains("vertex") {
        FormatFamily::Google
    } else if lower.contains("codex") || model_id.contains("codex") {
        FormatFamily::OpenAIResponses
    } else {
        FormatFamily::OpenAI
    }
}

fn capabilities_for_provider(provider_id: &str) -> ProviderCapabilities {
    let lower = provider_id.to_lowercase();
    if lower.contains("synthetic") || lower.contains("local") {
        ProviderCapabilities {
            streaming: false,
            max_tokens_default: None,
        }
    } else if lower.contains("anthropic") {
        ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(8192),
        }
    } else {
        ProviderCapabilities::default()
    }
}

fn auth_method_for_provider(provider_id: &str, api_key: &str) -> AuthMethod {
    if provider_id.to_lowercase().contains("anthropic") {
        AuthMethod::ApiKeyHeader {
            header: "x-api-key".to_string(),
            key: api_key.to_string(),
        }
    } else {
        AuthMethod::BearerToken(api_key.to_string())
    }
}

fn default_base_url(provider_id: &str) -> String {
    let lower = provider_id.to_lowercase();
    if lower.contains("anthropic") {
        "https://api.anthropic.com".to_string()
    } else if lower.contains("google") || lower.contains("gemini") {
        "https://generativelanguage.googleapis.com".to_string()
    } else {
        "https://api.openai.com".to_string()
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

trait StringExt {
    fn if_empty_then<F: FnOnce() -> String>(self, fallback: F) -> String;
}

impl StringExt for String {
    fn if_empty_then<F: FnOnce() -> String>(self, fallback: F) -> String {
        if self.is_empty() { fallback() } else { self }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};

    use anyhow::anyhow;
    use futures::{Stream, stream};
    use serde_json::Value;

    use super::*;
    use crate::provider::ToolChoice;

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
            let behavior = self.behaviors.lock().unwrap().remove(0);
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
            ProviderBehavior::Stream(vec![Ok(StreamEvent::Delta(ContentBlock::text("retry ok"))), Ok(StreamEvent::Done)]),
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
        assert!(error.to_string().contains("model unavailable") || error.to_string().contains("missing credentials") || error.to_string().contains("provider unavailable"));
    }
}
