use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::custom_provider::CustomProviderRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::models::provider::{CustomProvider, Model, Provider, SeedModel};
use crate::provider::health::ModelHealth;
use crate::provider::validate::{self, ValidationRequest};
use goose::config::Config as GooseConfig;
use goose::config::paths::Paths as GoosePaths;
use goose::providers::base::{ProviderMetadata, ProviderType};

// ── Shared response helpers ───────────────────────────────────────────────────

fn model_to_output(m: &Model) -> ProviderModelOutput {
    ProviderModelOutput {
        id: m.id.clone(),
        provider_id: m.provider_id.clone(),
        name: m.name.clone(),
        tool_call: m.tool_call,
        reasoning: m.reasoning,
        attachment: m.attachment,
        context_window: m.context_window,
        output_limit: m.output_limit,
        pricing: ModelPricingOutput {
            input_per_million: m.pricing.input_per_million,
            output_per_million: m.pricing.output_per_million,
            cache_read_per_million: m.pricing.cache_read_per_million,
            cache_write_per_million: m.pricing.cache_write_per_million,
        },
    }
}

pub(crate) fn canonical_provider_id(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) async fn goose_provider_ids() -> HashSet<String> {
    goose::providers::providers()
        .await
        .into_iter()
        .map(|(meta, _)| meta.name)
        .collect()
}

pub(crate) async fn goose_provider_entries() -> Vec<(ProviderMetadata, ProviderType)> {
    goose::providers::providers().await
}

pub(crate) fn resolve_goose_provider_name(
    provider_id: &str,
    entries: &[(ProviderMetadata, ProviderType)],
) -> Option<String> {
    if let Some((meta, _)) = entries.iter().find(|(meta, _)| meta.name == provider_id) {
        return Some(meta.name.clone());
    }

    let canonical = canonical_provider_id(provider_id);
    entries
        .iter()
        .find(|(meta, _)| canonical_provider_id(&meta.name) == canonical)
        .map(|(meta, _)| meta.name.clone())
}

pub(crate) fn oauth_keys_for_provider(
    provider_id: &str,
    entries: &[(ProviderMetadata, ProviderType)],
) -> Vec<String> {
    let Some(name) = resolve_goose_provider_name(provider_id, entries) else {
        return vec![];
    };

    entries
        .iter()
        .find(|(meta, _)| meta.name == name)
        .map(|(meta, _)| {
            meta.config_keys
                .iter()
                .filter(|k| k.oauth_flow)
                .map(|k| k.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Check whether any of the given OAuth keys have a stored token.
pub(crate) fn is_oauth_key_present(oauth_keys: &[String]) -> bool {
    oauth_keys.iter().any(|key| {
        if GooseConfig::global().get_secret::<String>(key).is_ok() {
            return true;
        }
        if key == "CHATGPT_CODEX_TOKEN" {
            return GoosePaths::in_config_dir("chatgpt_codex/tokens.json").exists();
        }
        false
    })
}

fn provider_connection_status(
    provider: &Provider,
    oauth_keys: &[String],
    credential_provider_ids: &HashSet<String>,
    credential_key_names: &HashSet<String>,
) -> (bool, Vec<&'static str>) {
    let credential_connected = credential_provider_ids.contains(&provider.id)
        || provider
            .env_vars
            .iter()
            .any(|env| credential_key_names.contains(env));

    let oauth_connected = !oauth_keys.is_empty() && is_oauth_key_present(oauth_keys);

    let mut methods = Vec::new();
    if credential_connected {
        methods.push("credential");
    }
    if oauth_connected {
        methods.push("oauth");
    }
    (!methods.is_empty(), methods)
}

pub(crate) async fn is_goose_builtin_provider(provider_id: &str) -> bool {
    let entries = goose_provider_entries().await;
    let Some(resolved_name) = resolve_goose_provider_name(provider_id, &entries) else {
        return false;
    };

    entries.into_iter().any(|(meta, ty)| {
        meta.name == resolved_name && matches!(ty, ProviderType::Builtin | ProviderType::Preferred)
    })
}

fn is_provider_usable_by_goose(provider: &Provider, goose_ids: &HashSet<String>) -> bool {
    provider.is_openai_compatible || goose_ids.contains(&provider.id)
}

// ── model_health ──────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ModelHealthInput {
    /// Action to perform: status (view all), reset (reset one model),
    /// reset_all (reset all models), enable (re-enable auto-disabled model).
    pub action: String,
    /// Model ID (required for reset and enable actions).
    pub model: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ModelHealthResponse {
    pub action: String,
    pub models: Vec<ModelHealthOutput>,
    pub error: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ModelHealthOutput {
    pub model_id: String,
    pub auto_disabled: bool,
    #[schemars(with = "i64")]
    pub consecutive_failures: u32,
    #[schemars(with = "i64")]
    pub total_failures: u32,
    #[schemars(with = "i64")]
    pub total_successes: u32,
    #[schemars(with = "i64")]
    pub disable_ttl_trips: u32,
    #[schemars(with = "Option<i64>")]
    pub cooldown_seconds_remaining: Option<u64>,
}

impl From<ModelHealth> for ModelHealthOutput {
    fn from(value: ModelHealth) -> Self {
        Self {
            model_id: value.model_id,
            auto_disabled: value.auto_disabled,
            consecutive_failures: value.consecutive_failures,
            total_failures: value.total_failures,
            total_successes: value.total_successes,
            disable_ttl_trips: value.disable_ttl_trips,
            cooldown_seconds_remaining: value.cooldown_seconds_remaining,
        }
    }
}

// ── provider_catalog ──────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProviderCatalogResponse {
    pub providers: Vec<ProviderCatalogItem>,
    pub total: i64,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderCatalogItem {
    pub id: String,
    pub name: String,
    pub npm: String,
    pub env_vars: Vec<String>,
    pub base_url: String,
    pub docs_url: String,
    pub is_openai_compatible: bool,
    pub connected: bool,
    pub oauth_supported: bool,
    pub oauth_keys: Vec<String>,
    pub connection_methods: Vec<String>,
}

// ── provider_connected ────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProviderConnectedResponse {
    pub providers: Vec<ProviderCatalogItem>,
    pub total: i64,
}

// ── provider_models ───────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProviderModelsInput {
    /// Provider ID to fetch models for (e.g. 'anthropic', 'openai').
    pub provider_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderModelsResponse {
    pub provider_id: String,
    pub models: Vec<ProviderModelOutput>,
    pub total: i64,
}

#[derive(Serialize, JsonSchema)]
pub struct ModelPricingOutput {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderModelOutput {
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub tool_call: bool,
    pub reasoning: bool,
    pub attachment: bool,
    pub context_window: i64,
    pub output_limit: i64,
    pub pricing: ModelPricingOutput,
}

// ── provider_models_connected ─────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProviderModelsConnectedResponse {
    pub models: Vec<ProviderModelOutput>,
    pub total: i64,
}

// ── provider_oauth_start ──────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProviderOauthStartInput {
    /// Provider ID to start OAuth for (accepts catalog/goose aliases, e.g. 'github-copilot').
    pub provider_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderOauthStartResponse {
    pub ok: bool,
    pub success: bool,
    pub provider_id: String,
    pub goose_provider_id: Option<String>,
    pub oauth_supported: bool,
    pub configured_keys: Vec<String>,
    pub error: Option<String>,
}

// ── provider_model_lookup ─────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProviderModelLookupInput {
    /// Full model ID in 'providerID/modelID' format (e.g. 'anthropic/claude-opus-4-6').
    pub model_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderModelLookupResponse {
    pub model_id: String,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "ProviderModelOutput")]
    pub model: Option<ProviderModelOutput>,
}

// ── provider_validate ─────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProviderValidateInput {
    /// Provider API base URL (e.g. https://api.openai.com/v1). The probe appends /models.
    pub base_url: String,
    /// API key to validate.
    pub api_key: String,
    /// Optional provider identifier for logging/diagnostics.
    pub provider_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderValidateResponse {
    pub ok: bool,
    pub error_kind: String,
    pub error: String,
    pub models: Vec<String>,
    pub http_status: i64,
}

// ── provider_add_custom ───────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct SeedModelInput {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProviderAddCustomInput {
    /// Unique provider slug (e.g. 'my-llm'). Must not collide with models.dev IDs.
    pub id: String,
    /// Human-readable display name (e.g. 'My LLM').
    pub name: String,
    /// Root URL for OpenAI-compatible API calls (e.g. https://api.my-llm.com/v1).
    pub base_url: String,
    /// Environment variable name for the API key (e.g. MY_LLM_API_KEY).
    pub env_var: String,
    /// Optional seed models to pre-populate the model picker.
    pub seed_models: Option<Vec<SeedModelInput>>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderAddCustomResponse {
    pub ok: bool,
    pub success: bool,
    pub id: String,
    pub error: Option<String>,
}

// ── Tool router ───────────────────────────────────────────────────────────────

#[tool_router(router = provider_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// View and manage model health state. Actions: status (view all), reset (reset one model),
    /// reset_all (reset all models), enable (re-enable auto-disabled model).
    #[tool(
        description = "View and manage model health state. Actions: status (view all), reset (reset one model), reset_all (reset all models), enable (re-enable auto-disabled model)."
    )]
    pub async fn model_health(
        &self,
        Parameters(input): Parameters<ModelHealthInput>,
    ) -> Json<ModelHealthResponse> {
        let tracker = self.state.health_tracker();
        let action = input.action.as_str();

        match action {
            "status" => {
                let all = tracker.all_health();
                let models: Vec<ModelHealthOutput> =
                    all.into_iter().map(ModelHealthOutput::from).collect();
                Json(ModelHealthResponse {
                    action: "status".into(),
                    models,
                    error: None,
                })
            }
            "reset" => {
                if let Some(model_id) = &input.model {
                    tracker.reset(model_id);
                    self.state.persist_model_health_state().await;
                    let h = tracker.model_health(model_id);
                    Json(ModelHealthResponse {
                        action: "reset".into(),
                        models: vec![ModelHealthOutput::from(h)],
                        error: None,
                    })
                } else {
                    Json(ModelHealthResponse {
                        action: "reset".into(),
                        models: vec![],
                        error: Some("model parameter required for reset".into()),
                    })
                }
            }
            "reset_all" => {
                tracker.reset_all();
                self.state.persist_model_health_state().await;
                Json(ModelHealthResponse {
                    action: "reset_all".into(),
                    models: vec![],
                    error: None,
                })
            }
            "enable" => {
                if let Some(model_id) = &input.model {
                    tracker.enable(model_id);
                    self.state.persist_model_health_state().await;
                    let h = tracker.model_health(model_id);
                    Json(ModelHealthResponse {
                        action: "enable".into(),
                        models: vec![ModelHealthOutput::from(h)],
                        error: None,
                    })
                } else {
                    Json(ModelHealthResponse {
                        action: "enable".into(),
                        models: vec![],
                        error: Some("model parameter required for enable".into()),
                    })
                }
            }
            _ => Json(ModelHealthResponse {
                action: action.to_owned(),
                models: vec![],
                error: Some(format!(
                    "unknown action '{action}'; valid: status, reset, reset_all, enable"
                )),
            }),
        }
    }

    /// List all LLM providers from the models.dev catalog. Each entry includes connection
    /// metadata (env vars, base URL, OpenAI-compat flag) and a connected placeholder for
    /// the desktop to merge local credential state.
    #[tool(
        description = "List providers Goose can use. Includes built-ins, declarative providers, and OpenAI-compatible catalog providers."
    )]
    pub async fn provider_catalog(&self) -> Json<ProviderCatalogResponse> {
        let goose_ids = goose_provider_ids().await;
        let goose_entries = goose_provider_entries().await;
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.events().clone());
        let (credential_provider_ids, credential_key_names) = match credential_repo.list().await {
            Ok(creds) => {
                let provider_ids = creds.iter().map(|c| c.provider_id.clone()).collect();
                let key_names = creds.iter().map(|c| c.key_name.clone()).collect();
                (provider_ids, key_names)
            }
            Err(e) => {
                tracing::warn!(error = %e, "provider_catalog: failed to load credentials");
                (HashSet::new(), HashSet::new())
            }
        };

        let providers: Vec<ProviderCatalogItem> = self
            .state
            .catalog()
            .list_providers()
            .iter()
            .filter(|p| is_provider_usable_by_goose(p, &goose_ids))
            .map(|p| {
                let oauth_keys = oauth_keys_for_provider(&p.id, &goose_entries);
                let (connected, methods) = provider_connection_status(
                    p,
                    &oauth_keys,
                    &credential_provider_ids,
                    &credential_key_names,
                );
                ProviderCatalogItem {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    npm: p.npm.clone(),
                    env_vars: p.env_vars.clone(),
                    base_url: p.base_url.clone(),
                    docs_url: p.docs_url.clone(),
                    is_openai_compatible: p.is_openai_compatible,
                    connected,
                    oauth_supported: !oauth_keys.is_empty(),
                    oauth_keys,
                    connection_methods: methods.into_iter().map(str::to_string).collect(),
                }
            })
            .collect();
        let total = i64::try_from(providers.len()).unwrap_or(i64::MAX);
        Json(ProviderCatalogResponse { providers, total })
    }

    /// List only connected providers (those with a stored credential or OAuth token).
    #[tool(
        description = "List connected providers only. Returns providers that have a stored API key or active OAuth token."
    )]
    pub async fn provider_connected(&self) -> Json<ProviderConnectedResponse> {
        let goose_ids = goose_provider_ids().await;
        let goose_entries = goose_provider_entries().await;
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.events().clone());
        let (credential_provider_ids, credential_key_names) = match credential_repo.list().await {
            Ok(creds) => {
                let provider_ids = creds.iter().map(|c| c.provider_id.clone()).collect();
                let key_names = creds.iter().map(|c| c.key_name.clone()).collect();
                (provider_ids, key_names)
            }
            Err(e) => {
                tracing::warn!(error = %e, "provider_connected: failed to load credentials");
                (HashSet::new(), HashSet::new())
            }
        };

        let providers: Vec<ProviderCatalogItem> = self
            .state
            .catalog()
            .list_providers()
            .iter()
            .filter(|p| is_provider_usable_by_goose(p, &goose_ids))
            .filter_map(|p| {
                let oauth_keys = oauth_keys_for_provider(&p.id, &goose_entries);
                let (connected, methods) = provider_connection_status(
                    p,
                    &oauth_keys,
                    &credential_provider_ids,
                    &credential_key_names,
                );
                if !connected {
                    return None;
                }
                Some(ProviderCatalogItem {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    npm: p.npm.clone(),
                    env_vars: p.env_vars.clone(),
                    base_url: p.base_url.clone(),
                    docs_url: p.docs_url.clone(),
                    is_openai_compatible: p.is_openai_compatible,
                    connected: true,
                    oauth_supported: !oauth_keys.is_empty(),
                    oauth_keys,
                    connection_methods: methods.into_iter().map(str::to_string).collect(),
                })
            })
            .collect();
        let total = i64::try_from(providers.len()).unwrap_or(i64::MAX);
        Json(ProviderConnectedResponse { providers, total })
    }

    /// List all models for a provider. Each model includes capabilities
    /// (tool_call, reasoning, attachment), context limits, and per-million-token pricing.
    #[tool(
        description = "List models for a Goose-usable provider. Returns empty for providers Goose cannot use."
    )]
    pub async fn provider_models(
        &self,
        Parameters(input): Parameters<ProviderModelsInput>,
    ) -> Json<ProviderModelsResponse> {
        let goose_ids = goose_provider_ids().await;
        let provider = self
            .state
            .catalog()
            .list_providers()
            .into_iter()
            .find(|p| p.id == input.provider_id);
        let Some(provider) = provider else {
            return Json(ProviderModelsResponse {
                provider_id: input.provider_id,
                models: vec![],
                total: 0,
            });
        };
        if !is_provider_usable_by_goose(&provider, &goose_ids) {
            return Json(ProviderModelsResponse {
                provider_id: input.provider_id,
                models: vec![],
                total: 0,
            });
        }

        let models: Vec<ProviderModelOutput> = self
            .state
            .catalog()
            .list_models(&provider.id)
            .iter()
            .map(model_to_output)
            .collect();
        let total = i64::try_from(models.len()).unwrap_or(i64::MAX);
        Json(ProviderModelsResponse {
            provider_id: provider.id,
            models,
            total,
        })
    }

    /// List all models across all connected providers in a single call.
    #[tool(
        description = "List all available models across all connected providers. Returns models grouped by provider with capabilities and pricing."
    )]
    pub async fn provider_models_connected(&self) -> Json<ProviderModelsConnectedResponse> {
        let goose_ids = goose_provider_ids().await;
        let goose_entries = goose_provider_entries().await;
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.events().clone());
        let (credential_provider_ids, credential_key_names) = match credential_repo.list().await {
            Ok(creds) => {
                let provider_ids = creds.iter().map(|c| c.provider_id.clone()).collect();
                let key_names = creds.iter().map(|c| c.key_name.clone()).collect();
                (provider_ids, key_names)
            }
            Err(e) => {
                tracing::warn!(error = %e, "provider_models_connected: failed to load credentials");
                (HashSet::new(), HashSet::new())
            }
        };

        let connected_provider_ids: Vec<String> = self
            .state
            .catalog()
            .list_providers()
            .iter()
            .filter(|p| is_provider_usable_by_goose(p, &goose_ids))
            .filter(|p| {
                let oauth_keys = oauth_keys_for_provider(&p.id, &goose_entries);
                let (connected, _) = provider_connection_status(
                    p,
                    &oauth_keys,
                    &credential_provider_ids,
                    &credential_key_names,
                );
                connected
            })
            .map(|p| p.id.clone())
            .collect();

        let models: Vec<ProviderModelOutput> = connected_provider_ids
            .iter()
            .flat_map(|pid| {
                self.state
                    .catalog()
                    .list_models(pid)
                    .into_iter()
                    .map(|m| model_to_output(&m))
            })
            .collect();
        let total = i64::try_from(models.len()).unwrap_or(i64::MAX);
        Json(ProviderModelsConnectedResponse { models, total })
    }

    /// Start OAuth authentication flow for a Goose provider that supports OAuth.
    /// This is used by UI onboarding/settings flows to connect OAuth-backed providers.
    #[tool(
        description = "Start OAuth authentication flow for a provider that supports OAuth. Returns success when Goose stores the provider token."
    )]
    pub async fn provider_oauth_start(
        &self,
        Parameters(input): Parameters<ProviderOauthStartInput>,
    ) -> Json<ProviderOauthStartResponse> {
        let entries = goose_provider_entries().await;
        let Some(goose_provider_id) = resolve_goose_provider_name(&input.provider_id, &entries)
        else {
            return Json(ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id: input.provider_id,
                goose_provider_id: None,
                oauth_supported: false,
                configured_keys: vec![],
                error: Some("provider is not registered in Goose".into()),
            });
        };

        let oauth_keys = oauth_keys_for_provider(&goose_provider_id, &entries);
        if oauth_keys.is_empty() {
            return Json(ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id: input.provider_id,
                goose_provider_id: Some(goose_provider_id),
                oauth_supported: false,
                configured_keys: vec![],
                error: Some("provider does not support OAuth flow".into()),
            });
        }

        let provider =
            match goose::providers::create_with_default_model(&goose_provider_id, Vec::new()).await
            {
                Ok(p) => p,
                Err(e) => {
                    return Json(ProviderOauthStartResponse {
                        ok: false,
                        success: false,
                        provider_id: input.provider_id,
                        goose_provider_id: Some(goose_provider_id),
                        oauth_supported: true,
                        configured_keys: vec![],
                        error: Some(e.to_string()),
                    });
                }
            };

        match provider.configure_oauth().await {
            Ok(()) => Json(ProviderOauthStartResponse {
                ok: true,
                success: true,
                provider_id: input.provider_id,
                goose_provider_id: Some(goose_provider_id),
                oauth_supported: true,
                configured_keys: oauth_keys,
                error: None,
            }),
            Err(e) => Json(ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id: input.provider_id,
                goose_provider_id: Some(goose_provider_id),
                oauth_supported: true,
                configured_keys: vec![],
                error: Some(e.to_string()),
            }),
        }
    }

    /// Look up a single model by its full 'providerID/modelID' identifier.
    /// Returns the model object (with capabilities and pricing) or null when not found.
    #[tool(
        description = "Look up a single model by its full 'providerID/modelID' identifier. Returns the model object (with capabilities and pricing) or null when not found."
    )]
    pub async fn provider_model_lookup(
        &self,
        Parameters(input): Parameters<ProviderModelLookupInput>,
    ) -> Json<ProviderModelLookupResponse> {
        let model_id = input.model_id.clone();
        match self.state.catalog().find_model(&model_id) {
            Some(m) => Json(ProviderModelLookupResponse {
                model_id,
                found: true,
                model: Some(model_to_output(&m)),
            }),
            None => Json(ProviderModelLookupResponse {
                model_id,
                found: false,
                model: None,
            }),
        }
    }

    /// Test whether an API key is valid for a given provider endpoint. Returns ok=true
    /// when the key is accepted. Does NOT store credentials.
    #[tool(
        description = "Test whether an API key is valid for a given provider endpoint. Returns ok=true when the key is accepted. Does NOT store credentials."
    )]
    pub async fn provider_validate(
        &self,
        Parameters(input): Parameters<ProviderValidateInput>,
    ) -> Json<ProviderValidateResponse> {
        let result = validate::validate(ValidationRequest {
            base_url: input.base_url,
            api_key: input.api_key,
            provider_id: input.provider_id,
        })
        .await;

        Json(ProviderValidateResponse {
            ok: result.ok,
            error_kind: result.error_kind.to_string(),
            error: result.error,
            models: result.models,
            http_status: i64::from(result.http_status),
        })
    }

    /// Register an unlisted OpenAI-compatible provider with a base URL and API key
    /// environment variable. Persists to DB for use by the model picker and env injection.
    #[tool(
        description = "Register an unlisted OpenAI-compatible provider with a base URL and API key environment variable. Persists to disk for use by the model picker and env injection."
    )]
    pub async fn provider_add_custom(
        &self,
        Parameters(input): Parameters<ProviderAddCustomInput>,
    ) -> Json<ProviderAddCustomResponse> {
        // Validate ID format (basic sanity check).
        if input.id.is_empty() || input.id.contains('/') {
            return Json(ProviderAddCustomResponse {
                ok: false,
                success: false,
                id: input.id,
                error: Some("provider id must be non-empty and must not contain '/'".into()),
            });
        }

        // Validate base_url scheme.
        if !input.base_url.starts_with("http://") && !input.base_url.starts_with("https://") {
            return Json(ProviderAddCustomResponse {
                ok: false,
                success: false,
                id: input.id,
                error: Some("base_url must use http or https scheme".into()),
            });
        }

        let seed_models: Vec<SeedModel> = input
            .seed_models
            .unwrap_or_default()
            .into_iter()
            .map(|s| SeedModel {
                id: s.id,
                name: s.name,
            })
            .collect();

        let provider = CustomProvider {
            id: input.id.clone(),
            name: input.name.clone(),
            base_url: input.base_url.clone(),
            env_var: input.env_var.clone(),
            seed_models: seed_models.clone(),
            created_at: String::new(),
        };

        // Persist to DB.
        let repo = CustomProviderRepository::new(self.state.db().clone());
        if let Err(e) = repo.upsert(&provider).await {
            tracing::warn!(id = %input.id, error = %e, "provider_add_custom: DB upsert failed");
            return Json(ProviderAddCustomResponse {
                ok: false,
                success: false,
                id: input.id,
                error: Some(e.to_string()),
            });
        }

        // Add to in-memory catalog.
        let catalog_provider = Provider {
            id: input.id.clone(),
            name: input.name,
            npm: String::new(),
            env_vars: vec![input.env_var],
            base_url: input.base_url,
            docs_url: String::new(),
            is_openai_compatible: true,
        };
        let catalog_models: Vec<Model> = seed_models
            .iter()
            .map(|s| Model {
                id: s.id.clone(),
                provider_id: input.id.clone(),
                name: s.name.clone(),
                tool_call: false,
                reasoning: false,
                attachment: false,
                context_window: 0,
                output_limit: 0,
                pricing: crate::models::provider::Pricing::default(),
            })
            .collect();
        self.state
            .catalog()
            .add_custom_provider(catalog_provider, catalog_models);

        tracing::info!(id = %input.id, "registered custom provider");
        Json(ProviderAddCustomResponse {
            ok: true,
            success: true,
            id: input.id,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, is_openai_compatible: bool) -> Provider {
        Provider {
            id: id.to_string(),
            name: id.to_string(),
            npm: String::new(),
            env_vars: vec![],
            base_url: String::new(),
            docs_url: String::new(),
            is_openai_compatible,
        }
    }

    #[test]
    fn goose_usable_provider_includes_openai_compatible_catalog_entries() {
        let p = provider("synthetic", true);
        let ids = HashSet::new();
        assert!(is_provider_usable_by_goose(&p, &ids));
    }

    #[test]
    fn goose_usable_provider_includes_goose_registered_ids() {
        let p = provider("anthropic", false);
        let ids = HashSet::from(["anthropic".to_string()]);
        assert!(is_provider_usable_by_goose(&p, &ids));
    }

    #[test]
    fn goose_usable_provider_excludes_unsupported_non_openai_compatible_entries() {
        let p = provider("unknown", false);
        let ids = HashSet::new();
        assert!(!is_provider_usable_by_goose(&p, &ids));
    }

    #[test]
    fn canonical_provider_id_removes_separators() {
        assert_eq!(canonical_provider_id("github-copilot"), "githubcopilot");
        assert_eq!(canonical_provider_id("github_copilot"), "githubcopilot");
    }

    #[test]
    fn resolve_goose_provider_name_matches_canonical_alias() {
        let entries = vec![
            (
                ProviderMetadata::new("githubcopilot", "", "", "", vec![], "", vec![]),
                ProviderType::Builtin,
            ),
            (
                ProviderMetadata::new("openai", "", "", "", vec![], "", vec![]),
                ProviderType::Preferred,
            ),
        ];
        assert_eq!(
            resolve_goose_provider_name("github-copilot", &entries),
            Some("githubcopilot".to_string())
        );
    }

    #[test]
    fn provider_connection_status_marks_credential_connection() {
        let p = provider("openai", true);
        let credential_provider_ids = HashSet::from(["openai".to_string()]);
        let credential_key_names = HashSet::new();
        let (connected, methods) =
            provider_connection_status(&p, &[], &credential_provider_ids, &credential_key_names);
        assert!(connected);
        assert_eq!(methods, vec!["credential"]);
    }
}
