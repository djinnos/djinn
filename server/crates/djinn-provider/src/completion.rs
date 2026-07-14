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
    let (provider_config, _) = resolve_memory_provider_config_for_user_db(db, user_id).await?;
    Ok(create_provider(provider_config))
}

/// Resolve the memory provider configuration and selected catalog model id
/// under an explicit caller credential scope without constructing a provider.
pub async fn resolve_memory_provider_config_for_user_db(
    db: &Database,
    user_id: Option<&str>,
) -> Result<(ProviderConfig, String)> {
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
        && let Some(lanes) = user_settings.lanes
    {
        // Memory enrichment is not role-scoped; any model the user selected in
        // any lane is a fair candidate (union, dedup, lane order).
        model_candidates.extend(lanes.all_models());
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

    let model_id = provider_config.model_id.clone();
    Ok((provider_config, model_id))
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
        tool_schema_compat: builtin::tool_schema_compat_for(provider_id, &model.id),
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

// djinn:allow-oversize — the bulk of this file is the inline #[cfg(test)] suite
// below. It is deliberately kept inline (not split into a sibling *_tests.rs):
// memory_resolver_grep_guard skips test code by tracking the `#[cfg(test)] mod
// tests {` brace block, and a #[path]-attached sibling file would be scanned as
// production code, mis-flagging the resolve_memory_provider test call sites.
#[cfg(test)]
mod tests {
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
        ToolSchemaCompat,
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
                "djinn-slot/src/llm_extraction.rs",
                include_str!("../../djinn-slot/src/llm_extraction.rs"),
            ),
            (
                "djinn-control-plane/src/tools/memory_tools/summaries.rs",
                include_str!("../../djinn-control-plane/src/tools/memory_tools/summaries.rs"),
            ),
            (
                "djinn-control-plane/src/tools/memory_tools/write_dedup_runtime.rs",
                include_str!(
                    "../../djinn-control-plane/src/tools/memory_tools/write_dedup_runtime.rs"
                ),
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
        let builtin_provider = builtin::find_builtin_provider("anthropic")
            .expect("anthropic provider row should exist");
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

    #[test]
    fn api_key_provider_config_sets_tool_schema_compat() {
        let kimi = test_model("kimi-for-coding", "k2p7", true);
        let minimax = test_model("minimax-coding-plan", "MiniMax-M3", true);
        let google = test_model("google", "gemini-2.5-pro", true);
        let openai = test_model("openai", "gpt-4.1-mini", true);
        let anthropic = test_model("anthropic", "claude-3-5-haiku", false);

        assert_eq!(
            api_key_provider_config(
                "kimi-for-coding",
                &kimi,
                builtin::find_builtin_provider("kimi-for-coding").expect("kimi builtin"),
                "kimi-secret".to_string(),
            )
            .tool_schema_compat,
            Some(ToolSchemaCompat::Moonshot)
        );
        assert_eq!(
            api_key_provider_config(
                "minimax-coding-plan",
                &minimax,
                builtin::find_builtin_provider("minimax-coding-plan").expect("minimax builtin"),
                "minimax-secret".to_string(),
            )
            .tool_schema_compat,
            Some(ToolSchemaCompat::Moonshot)
        );
        assert_eq!(
            api_key_provider_config(
                "google",
                &google,
                builtin::find_builtin_provider("google").expect("google builtin"),
                "google-secret".to_string(),
            )
            .tool_schema_compat,
            Some(ToolSchemaCompat::Gemini)
        );
        assert_eq!(
            api_key_provider_config(
                "openai",
                &openai,
                builtin::find_builtin_provider("openai").expect("openai builtin"),
                "openai-secret".to_string(),
            )
            .tool_schema_compat,
            None
        );
        assert_eq!(
            api_key_provider_config(
                "anthropic",
                &anthropic,
                builtin::find_builtin_provider("anthropic").expect("anthropic builtin"),
                "anthropic-secret".to_string(),
            )
            .tool_schema_compat,
            None
        );
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
                        let stream: Pin<
                            Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                        > = Box::pin(stream::iter(events));
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
            .upsert_lanes(
                &user.id,
                &djinn_core::models::ModelLanes::from_flat(vec!["openai/gpt-4.1-mini".to_string()]),
            )
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
            .upsert_lanes(
                &user.id,
                &djinn_core::models::ModelLanes::from_flat(vec!["openai/gpt-4.1-mini".to_string()]),
            )
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
            .upsert_lanes(
                &user.id,
                &djinn_core::models::ModelLanes::from_flat(vec!["openai/gpt-4.1-mini".to_string()]),
            )
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
                id: "xiaomi-token-plan-sgp",
                fixed_family: Some(FormatFamily::OpenAI),
                auth_shape: AuthShape::Bearer,
                streaming: true,
                max_tokens_default: None,
            },
            Expected {
                id: "kimi-for-coding",
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
}
