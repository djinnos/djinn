use anyhow::{Context, Result, anyhow};
use djinn_core::events::EventBus;
use djinn_core::message::{ContentBlock, Conversation, Message};
use djinn_core::models::{Credential, DjinnSettings, Model};
use djinn_db::{Database, SettingsRepository, UserSettingsRepository};
use futures::StreamExt;
use tokio::time::{Duration, timeout};

use crate::catalog::{CatalogService, builtin};
use crate::oauth::{self, codex::CodexTokens, copilot::CopilotTokens};
use crate::provider::{
    LlmProvider, ProviderConfig, StreamEvent, TokenUsage, create_provider,
    default_reasoning_effort_for_model,
};
use crate::repos::CredentialRepository;

const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const SETTINGS_RAW_KEY: &str = "settings.raw";

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
    let candidates = selected_model_id.into_iter().map(str::to_string);
    select_memory_model_from_candidates(catalog, credentials, candidates)
}

pub(crate) fn select_memory_model_from_candidates<I>(
    catalog: &CatalogService,
    credentials: &[Credential],
    selected_model_ids: I,
) -> Result<ResolvedMemoryModel>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let connected = catalog.connected_provider_ids(credentials);

    let mut first_candidate_error = None;
    for item in selected_model_ids {
        let model_id = item.as_ref().trim();
        if model_id.is_empty() {
            continue;
        }

        if let Some((provider_id, _)) = model_id.split_once('/')
            && !connected.contains(provider_id)
        {
            continue;
        }

        let Some(model) = catalog.find_model(model_id) else {
            first_candidate_error.get_or_insert_with(|| {
                anyhow!(
                    "memory.llm_model '{}' is not available in the provider catalog",
                    model_id
                )
            });
            continue;
        };
        match effective_provider_for_model(&model, credentials) {
            Ok(effective_provider_id) => {
                return Ok(ResolvedMemoryModel {
                    model,
                    effective_provider_id,
                });
            }
            Err(error) => {
                first_candidate_error.get_or_insert(error);
            }
        }
    }

    if let Some(error) = first_candidate_error {
        return Err(error);
    }

    let mut candidates = builtin::BUILTIN_PROVIDERS
        .iter()
        .flat_map(|provider| catalog.list_models(provider.id).into_iter())
        .filter(|model| connected.contains(&model.provider_id))
        .filter(|model| model.tool_call) // exclude embedding / non-chat models
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        total_price(left)
            .partial_cmp(&total_price(right))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.provider_id.cmp(&right.provider_id))
            .then_with(|| left.id.cmp(&right.id))
    });

    let model = candidates.into_iter().next().ok_or_else(|| {
        anyhow!("no connected builtin provider models are available for memory.llm_model fallback")
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
                let error = anyhow!(
                    "completion timed out after {}s",
                    COMPLETION_TIMEOUT.as_secs()
                );
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
            StreamEvent::Delta(ContentBlock::Text { text: delta, .. }) => text.push_str(&delta),
            StreamEvent::Usage(token_usage) => usage = token_usage,
            StreamEvent::Done => break,
            StreamEvent::Delta(_) | StreamEvent::Thinking(_) => {}
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

/// Resolve the memory LLM provider for the current task-local caller scope.
///
/// With `Some(user)` from `djinn_core::auth_context::current_user_id()`, the
/// resolver can see that user's own credentials plus org-shared fallback
/// credentials. With no current user (background/no-session work), it can see
/// only org-shared credentials. This compatibility wrapper never falls back to
/// all-owner credential listing.
pub async fn resolve_memory_provider(db: &Database) -> Result<Box<dyn LlmProvider>> {
    let user_id = djinn_core::auth_context::current_user_id();
    resolve_memory_provider_for_user(db, user_id.as_deref()).await
}

/// Resolve the memory LLM provider under an explicit credential visibility
/// scope.
///
/// `user_id = Some(user)` means the user's own credentials plus org-shared
/// fallback credentials are visible. `user_id = None` is the background/
/// no-session scope and can see only org-shared credentials.
pub async fn resolve_memory_provider_for_user(
    db: &Database,
    user_id: Option<&str>,
) -> Result<Box<dyn LlmProvider>> {
    let event_bus = EventBus::noop();
    let settings_repo = SettingsRepository::new(db.clone(), event_bus.clone());

    // Read unified settings from DB.
    let settings_raw = settings_repo
        .get(SETTINGS_RAW_KEY)
        .await?
        .map(|s| s.value)
        .unwrap_or_default();
    let settings = DjinnSettings::from_db_value(&settings_raw);

    let mut model_candidates = Vec::new();
    if let Some(uid) = user_id
        && let Some(user_settings) = UserSettingsRepository::new(db.clone()).get(uid).await?
        && let Some(models) = user_settings.models
    {
        model_candidates.extend(models);
    }
    model_candidates.extend(settings.models_or_default());
    if model_candidates.is_empty() {
        return Err(anyhow!(
            "no model configured — add a model in Settings → Model Configuration"
        ));
    }

    let catalog = CatalogService::new();
    catalog.inject_builtin_providers(builtin::BUILTIN_PROVIDERS);

    let credential_repo = CredentialRepository::new(db.clone(), event_bus);
    let credentials = credential_repo.list_for_user(user_id).await?;
    let provider_config = resolve_memory_provider_config_for_candidates(
        &catalog,
        &credentials,
        &credential_repo,
        model_candidates,
        user_id,
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
    let user_id = djinn_core::auth_context::current_user_id();
    resolve_memory_provider_config_for_user(
        catalog,
        credentials,
        credential_repo,
        settings_raw,
        user_id.as_deref(),
    )
    .await
}

pub async fn resolve_memory_provider_config_for_user(
    catalog: &CatalogService,
    credentials: &[Credential],
    credential_repo: &CredentialRepository,
    settings_raw: &str,
    user_id: Option<&str>,
) -> Result<ProviderConfig> {
    let selected = parse_memory_model_selection(settings_raw);
    resolve_memory_provider_config_for_candidates(
        catalog,
        credentials,
        credential_repo,
        selected,
        user_id,
    )
    .await
}

async fn resolve_memory_provider_config_for_candidates<I>(
    catalog: &CatalogService,
    credentials: &[Credential],
    credential_repo: &CredentialRepository,
    model_candidates: I,
    user_id: Option<&str>,
) -> Result<ProviderConfig>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let resolved = match select_memory_model_from_candidates(catalog, credentials, model_candidates)
    {
        Ok(resolved) => resolved,
        Err(primary_err) => {
            // The configured memory model couldn't be resolved against a
            // CONNECTED provider. This is common: `resolve_memory_provider`
            // feeds the first worker model (e.g. a codex-served `openai/gpt-5.x`
            // id) which the static models.dev catalog doesn't list, even though
            // dispatch runs it fine via the connected `chatgpt_codex`
            // credential. Erroring here skips post-session knowledge extraction
            // ENTIRELY (zero scoped notes on the whole deployment). Fall back to
            // the cheapest connected model so extraction still runs.
            select_memory_model(catalog, credentials, None).map_err(|fallback_err| {
                anyhow!(
                    "memory model unresolvable ({primary_err}); connected-model fallback also failed: {fallback_err}"
                )
            })?
        }
    };
    provider_config_for_model_for_user(&resolved, credential_repo, user_id).await
}

#[cfg(test)]
pub(crate) async fn provider_config_for_model(
    resolved: &ResolvedMemoryModel,
    credential_repo: &CredentialRepository,
) -> Result<ProviderConfig> {
    provider_config_for_model_for_user(resolved, credential_repo, None).await
}

pub(crate) async fn provider_config_for_model_for_user(
    resolved: &ResolvedMemoryModel,
    credential_repo: &CredentialRepository,
    user_id: Option<&str>,
) -> Result<ProviderConfig> {
    match resolved.effective_provider_id.as_str() {
        "chatgpt_codex" => {
            let tokens = CodexTokens::load_from_db_for_user(credential_repo, user_id)
                .await
                .ok_or_else(|| {
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
            let tokens = CopilotTokens::load_from_db_for_user(credential_repo, user_id)
                .await
                .ok_or_else(|| {
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
        provider_id => {
            api_key_provider_config_for_user(provider_id, &resolved.model, credential_repo, user_id)
                .await
        }
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

async fn api_key_provider_config_for_user(
    provider_id: &str,
    model: &Model,
    credential_repo: &CredentialRepository,
    user_id: Option<&str>,
) -> Result<ProviderConfig> {
    let builtin_provider = builtin::find_builtin_provider(provider_id).ok_or_else(|| {
        anyhow!(
            "provider '{}' is not supported by djinn-provider",
            provider_id
        )
    })?;
    let key_name = builtin_provider.required_env_vars.first().ok_or_else(|| {
        anyhow!(
            "provider '{}' for memory model '{}' does not support API-key auth",
            provider_id,
            model.id
        )
    })?;
    let api_key = credential_repo
        .get_decrypted_for_user(key_name, user_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "provider '{}' for memory model '{}' is missing credential '{}'",
                provider_id,
                model.id,
                key_name
            )
        })?;

    Ok(api_key_provider_config(
        provider_id,
        model,
        builtin_provider,
        api_key,
    ))
}

fn api_key_provider_config(
    provider_id: &str,
    model: &Model,
    builtin_provider: &builtin::BuiltinProvider,
    api_key: String,
) -> ProviderConfig {
    // G8: format_family / auth shape / capabilities are now carried on the
    // `BuiltinProvider` row (see `catalog::builtin`) instead of three separate
    // per-provider `match` arms keyed on the provider id. The row was already
    // resolved above as `builtin_provider`, so these are direct lookups.
    let format_family = builtin_provider.format_family(&model.id);

    ProviderConfig {
        base_url: provider_base_url(provider_id),
        auth: builtin_provider.auth_method(api_key),
        format_family,
        model_id: model.id.clone(),
        context_window: model.context_window.max(0) as u32,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: builtin_provider.capabilities(),
        reasoning_effort: default_reasoning_effort_for_model(
            model.reasoning,
            format_family,
            &model.id,
        ),
    }
}

fn provider_base_url(provider_id: &str) -> String {
    match provider_id {
        "anthropic" => "https://api.anthropic.com".to_string(),
        "openai" => "https://api.openai.com".to_string(),
        "google" => "https://generativelanguage.googleapis.com".to_string(),
        _ => "https://api.openai.com".to_string(),
    }
}

fn is_transient_error(error: &anyhow::Error) -> bool {
    // Prefer the typed provider taxonomy attached at the provider-crate
    // boundary; fall back to substring matching for untyped/legacy errors.
    if let Some(pe) = error.downcast_ref::<crate::provider::error::ProviderError>() {
        return pe.retryable();
    }
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
#[path = "completion_tests.rs"]
mod tests;
