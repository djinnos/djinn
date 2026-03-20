use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::server::DjinnMcpServer;
use djinn_core::models::{CustomProvider, Model, Provider, SeedModel};
use djinn_provider::catalog::builtin;
use djinn_provider::catalog::health::ModelHealth;
use djinn_provider::catalog::validate::{self, ValidationRequest};
use djinn_provider::repos::CredentialRepository;
use djinn_provider::repos::CustomProviderRepository;

// ── Shared response helpers ───────────────────────────────────────────────────

fn model_to_output(m: &Model) -> ProviderModelOutput {
    // Always return the full "provider/model" form for API consumers.
    // Internal IDs may be bare after normalization.
    let full_id = if m.id.contains('/') {
        m.id.clone()
    } else {
        format!("{}/{}", m.provider_id, m.id)
    };
    ProviderModelOutput {
        id: full_id,
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

    let oauth_connected =
        !oauth_keys.is_empty() && builtin::is_oauth_key_present(oauth_keys, credential_key_names);

    let mut methods = Vec::new();
    if credential_connected {
        methods.push("credential");
    }
    if oauth_connected {
        methods.push("oauth");
    }
    (!methods.is_empty(), methods)
}

fn is_provider_usable(provider: &Provider, builtin_ids: &HashSet<String>) -> bool {
    provider.is_openai_compatible || builtin_ids.contains(&provider.id)
}

// ── model_health ──────────────────────────────────────────────────────────────

fn default_model_health_action() -> String {
    "status".to_string()
}

#[derive(Deserialize, JsonSchema)]
pub struct ModelHealthInput {
    /// Action to perform: status (view all, default), reset (reset one model),
    /// reset_all (reset all models), enable (re-enable auto-disabled model).
    #[serde(default = "default_model_health_action")]
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
    pub builtin_id: String,
    #[serde(rename = "goose_provider_id")]
    pub legacy_builtin_id: String,
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
    /// Provider ID to start OAuth for (accepts catalog aliases, e.g. 'github-copilot').
    pub provider_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderOauthStartResponse {
    pub ok: bool,
    pub success: bool,
    pub provider_id: String,
    pub builtin_id: Option<String>,
    #[serde(rename = "goose_provider_id")]
    pub legacy_builtin_id: Option<String>,
    pub oauth_supported: bool,
    pub configured_keys: Vec<String>,
    pub error: Option<String>,
    /// For device-code flows: the code the user must enter.
    pub user_code: Option<String>,
    /// For device-code flows: the URL where the user enters the code.
    pub verification_uri: Option<String>,
    /// True when the flow is still in progress (device-code polling in background).
    pub pending: bool,
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
    /// When omitted, the server resolves it from the catalog using provider_id.
    pub base_url: Option<String>,
    /// API key to validate.
    pub api_key: String,
    /// Provider identifier. Used for logging and to resolve base_url from the catalog when not supplied.
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

// ── provider_remove ───────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProviderRemoveInput {
    /// Provider ID to disconnect and remove (e.g. 'anthropic', 'openai', 'my-custom-llm').
    pub provider_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProviderRemoveResponse {
    pub ok: bool,
    pub success: bool,
    pub provider_id: String,
    pub credentials_deleted: i64,
    pub custom_provider_deleted: bool,
    pub oauth_cleared: bool,
    pub error: Option<String>,
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
        description = "List providers Djinn can use. Includes built-ins, custom providers, and OpenAI-compatible catalog providers."
    )]
    pub async fn provider_catalog(&self) -> Json<ProviderCatalogResponse> {
        let builtin_ids = builtin::builtin_provider_ids();
        let merged_ids = builtin::merged_provider_ids();
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());
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
            .filter(|p| is_provider_usable(p, &builtin_ids))
            // Hide providers that are merged into a parent (e.g. chatgpt_codex → openai).
            .filter(|p| !merged_ids.contains(&p.id))
            .map(|p| {
                let oauth_keys = builtin::all_oauth_keys_for_provider(&p.id);
                let (connected, methods) = provider_connection_status(
                    p,
                    &oauth_keys,
                    &credential_provider_ids,
                    &credential_key_names,
                );
                ProviderCatalogItem {
                    id: p.id.clone(),
                    builtin_id: p.id.clone(),
                    legacy_builtin_id: p.id.clone(),
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
        let builtin_ids = builtin::builtin_provider_ids();
        let merged_ids = builtin::merged_provider_ids();
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());
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
            .filter(|p| is_provider_usable(p, &builtin_ids))
            .filter(|p| !merged_ids.contains(&p.id))
            .filter_map(|p| {
                let oauth_keys = builtin::all_oauth_keys_for_provider(&p.id);
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
                    builtin_id: p.id.clone(),
                    legacy_builtin_id: p.id.clone(),
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
    #[tool(description = "List models for a provider. Returns empty for unknown providers.")]
    pub async fn provider_models(
        &self,
        Parameters(input): Parameters<ProviderModelsInput>,
    ) -> Json<ProviderModelsResponse> {
        let builtin_ids = builtin::builtin_provider_ids();
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
        if !is_provider_usable(&provider, &builtin_ids) {
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
        let builtin_ids = builtin::builtin_provider_ids();
        let merged_ids = builtin::merged_provider_ids();
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());
        let credentials = credential_repo.list().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "provider_models_connected: failed to load credentials");
            Vec::new()
        });
        let connected_set = self.state.catalog().connected_provider_ids(&credentials);

        // Collect connected provider IDs including merged children.
        let mut connected_provider_ids: Vec<String> = Vec::new();
        for p in self.state.catalog().list_providers().iter() {
            if !is_provider_usable(p, &builtin_ids) || !connected_set.contains(&p.id) {
                continue;
            }
            connected_provider_ids.push(p.id.clone());
            // If this parent has merged children, include their models too.
            if !merged_ids.is_empty() {
                for child_id in &merged_ids {
                    if builtin::find_builtin_provider(child_id).and_then(|bp| bp.merge_into)
                        == Some(p.id.as_str())
                    {
                        connected_provider_ids.push(child_id.clone());
                    }
                }
            }
        }

        let mut seen_ids: HashSet<String> = HashSet::new();
        let models: Vec<ProviderModelOutput> = connected_provider_ids
            .iter()
            .flat_map(|pid| {
                // For merged children, re-tag models with the parent provider ID
                // so the frontend sees a single provider namespace.
                let display_pid = builtin::find_builtin_provider(pid)
                    .and_then(|bp| bp.merge_into)
                    .unwrap_or(pid.as_str())
                    .to_string();
                self.state
                    .catalog()
                    .list_models(pid)
                    .into_iter()
                    .map(move |m| {
                        let mut out = model_to_output(&m);
                        out.provider_id = display_pid.clone();
                        // Re-tag the full ID to use the display provider
                        // (for merged children → parent namespace).
                        let bare = m.id.split_once('/').map(|(_, name)| name).unwrap_or(&m.id);
                        out.id = format!("{display_pid}/{bare}");
                        out
                    })
            })
            // Deduplicate: parent models listed first, merged children's models
            // are only added if not already present from the parent.
            .filter(|m| seen_ids.insert(m.id.clone()))
            .collect();
        let total = i64::try_from(models.len()).unwrap_or(i64::MAX);
        Json(ProviderModelsConnectedResponse { models, total })
    }

    /// Start OAuth authentication flow for a provider that supports OAuth.
    /// This is used by UI onboarding/settings flows to connect OAuth-backed providers.
    #[tool(
        description = "Start OAuth authentication flow for a provider that supports OAuth. Returns success when the provider token is stored."
    )]
    pub async fn provider_oauth_start(
        &self,
        Parameters(input): Parameters<ProviderOauthStartInput>,
    ) -> Json<ProviderOauthStartResponse> {
        use djinn_provider::oauth::{OAuthFlowKind, codex, copilot};

        let resolved_name = builtin::resolve_builtin_name(&input.provider_id);
        let Some(builtin_id) = resolved_name else {
            return Json(ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id: input.provider_id,
                builtin_id: None,
                legacy_builtin_id: None,
                oauth_supported: false,
                configured_keys: vec![],
                error: Some("provider is not a known built-in".into()),
                user_code: None,
                verification_uri: None,
                pending: false,
            });
        };

        // Resolve OAuth keys (own + merged children, e.g. "openai" inherits "chatgpt_codex" keys).
        let oauth_keys = builtin::all_oauth_keys_for_provider(builtin_id);
        let effective_id = if oauth_keys.is_empty() {
            builtin_id
        } else if builtin::oauth_keys_for_provider(builtin_id).is_empty() {
            // OAuth comes from a merged child — resolve to child for the actual flow.
            builtin::resolve_oauth_provider(builtin_id).unwrap_or(builtin_id)
        } else {
            builtin_id
        };

        if oauth_keys.is_empty() {
            return Json(ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id: input.provider_id,
                builtin_id: Some(builtin_id.to_string()),
                legacy_builtin_id: Some(builtin_id.to_string()),
                oauth_supported: false,
                configured_keys: vec![],
                error: Some("provider does not support OAuth flow".into()),
                user_code: None,
                verification_uri: None,
                pending: false,
            });
        }

        let flow_kind = OAuthFlowKind::from_provider_id(effective_id);
        let Some(flow_kind) = flow_kind else {
            return Json(ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id: input.provider_id,
                builtin_id: Some(builtin_id.to_string()),
                legacy_builtin_id: Some(builtin_id.to_string()),
                oauth_supported: true,
                configured_keys: vec![],
                error: Some(format!("no OAuth flow implemented for '{effective_id}'")),
                user_code: None,
                verification_uri: None,
                pending: false,
            });
        };

        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());

        // GitHub App uses a non-blocking device-code flow: return the code
        // immediately and poll in the background.
        if matches!(flow_kind, OAuthFlowKind::GitHubApp) {
            use djinn_provider::oauth::github_app;

            // If we already have a valid cached token, return success immediately.
            if let Some(cached) = github_app::GitHubAppTokens::load_from_db(&credential_repo).await
            {
                if !cached.is_expired() {
                    return Json(ProviderOauthStartResponse {
                        ok: true,
                        success: true,
                        provider_id: input.provider_id,
                        builtin_id: Some(builtin_id.to_string()),
                        legacy_builtin_id: Some(builtin_id.to_string()),
                        oauth_supported: true,
                        configured_keys: oauth_keys,
                        error: None,
                        user_code: None,
                        verification_uri: None,
                        pending: false,
                    });
                }
                // Try refresh before falling through to device flow
                if let Ok(tokens) = github_app::refresh_cached_token(
                    &cached,
                    github_app::CLIENT_ID,
                    &credential_repo,
                )
                .await
                {
                    let _ = tokens; // already saved by refresh_cached_token
                    return Json(ProviderOauthStartResponse {
                        ok: true,
                        success: true,
                        provider_id: input.provider_id,
                        builtin_id: Some(builtin_id.to_string()),
                        legacy_builtin_id: Some(builtin_id.to_string()),
                        oauth_supported: true,
                        configured_keys: oauth_keys,
                        error: None,
                        user_code: None,
                        verification_uri: None,
                        pending: false,
                    });
                }
            }

            return match github_app::start_device_flow().await {
                Ok(session) => {
                    let user_code = session.user_code.clone();
                    let verification_uri = session
                        .verification_uri_complete
                        .clone()
                        .unwrap_or_else(|| session.verification_uri.clone());

                    // Spawn background polling task
                    let bg_db = self.state.db().clone();
                    let bg_events = self.state.event_bus();
                    tokio::spawn(async move {
                        let bg_repo = CredentialRepository::new(bg_db, bg_events);
                        if let Err(e) = github_app::poll_and_store(&session, &bg_repo).await {
                            tracing::error!("GitHubApp background poll failed: {}", e);
                        }
                    });

                    Json(ProviderOauthStartResponse {
                        ok: true,
                        success: false,
                        provider_id: input.provider_id,
                        builtin_id: Some(builtin_id.to_string()),
                        legacy_builtin_id: Some(builtin_id.to_string()),
                        oauth_supported: true,
                        configured_keys: oauth_keys,
                        error: None,
                        user_code: Some(user_code),
                        verification_uri: Some(verification_uri),
                        pending: true,
                    })
                }
                Err(e) => Json(ProviderOauthStartResponse {
                    ok: false,
                    success: false,
                    provider_id: input.provider_id,
                    builtin_id: Some(builtin_id.to_string()),
                    legacy_builtin_id: Some(builtin_id.to_string()),
                    oauth_supported: true,
                    configured_keys: vec![],
                    error: Some(e.to_string()),
                    user_code: None,
                    verification_uri: None,
                    pending: false,
                }),
            };
        }

        // Blocking flows (Codex, Copilot)
        let result = match flow_kind {
            OAuthFlowKind::Codex => codex::run_codex_flow(&credential_repo).await.map(|_| ()),
            OAuthFlowKind::Copilot => match copilot::start_copilot_flow().await {
                Ok(session) => copilot::poll_copilot_flow(session, &credential_repo)
                    .await
                    .map(|_| ()),
                Err(e) => Err(e),
            },
            OAuthFlowKind::GitHubApp => unreachable!(),
        };

        match result {
            Ok(()) => Json(ProviderOauthStartResponse {
                ok: true,
                success: true,
                provider_id: input.provider_id,
                builtin_id: Some(builtin_id.to_string()),
                legacy_builtin_id: Some(builtin_id.to_string()),
                oauth_supported: true,
                configured_keys: oauth_keys,
                error: None,
                user_code: None,
                verification_uri: None,
                pending: false,
            }),
            Err(e) => Json(ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id: input.provider_id,
                builtin_id: Some(builtin_id.to_string()),
                legacy_builtin_id: Some(builtin_id.to_string()),
                oauth_supported: true,
                configured_keys: vec![],
                error: Some(e.to_string()),
                user_code: None,
                verification_uri: None,
                pending: false,
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
        // Resolve base_url: use explicit value, fall back to catalog lookup, then known defaults.
        let base_url = match input.base_url.as_deref() {
            Some(url) if !url.is_empty() => url.to_string(),
            _ => {
                let from_catalog = input.provider_id.as_deref().and_then(|pid| {
                    self.state
                        .catalog()
                        .list_providers()
                        .into_iter()
                        .find(|p| p.id == pid)
                        .map(|p| p.base_url)
                        .filter(|u| !u.is_empty())
                });
                from_catalog.unwrap_or_else(|| {
                    // Well-known defaults for providers whose native API isn't OpenAI-compatible
                    // but still expose a /models-style list endpoint.
                    match input.provider_id.as_deref() {
                        Some("anthropic") => "https://api.anthropic.com/v1".to_string(),
                        _ => String::new(),
                    }
                })
            }
        };

        let result = validate::validate(ValidationRequest {
            base_url,
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
        let repo = CustomProviderRepository::new(self.state.db().clone(), self.state.event_bus());
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
                pricing: djinn_core::models::Pricing::default(),
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

    /// Fully disconnect a provider: delete all stored credentials, remove OAuth
    /// tokens, and delete custom provider entry if applicable. Single endpoint
    /// for the desktop to call when the user clicks "Remove".
    #[tool(
        description = "Fully disconnect a provider by ID. Deletes stored credentials, removes OAuth tokens, and deletes the custom provider entry if applicable."
    )]
    pub async fn provider_remove(
        &self,
        Parameters(input): Parameters<ProviderRemoveInput>,
    ) -> Json<ProviderRemoveResponse> {
        let provider_id = &input.provider_id;

        // 1. Delete all credentials for this provider.
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());
        let credentials_deleted = match credential_repo.delete_by_provider(provider_id).await {
            Ok(n) => i64::try_from(n).unwrap_or(i64::MAX),
            Err(e) => {
                tracing::warn!(provider_id = %provider_id, error = %e, "provider_remove: credential delete failed");
                return Json(ProviderRemoveResponse {
                    ok: false,
                    success: false,
                    provider_id: input.provider_id,
                    credentials_deleted: 0,
                    custom_provider_deleted: false,
                    oauth_cleared: false,
                    error: Some(format!("failed to delete credentials: {e}")),
                });
            }
        };

        // 2. Clear OAuth tokens (if this provider uses OAuth).
        let oauth_keys = builtin::all_oauth_keys_for_provider(provider_id);
        let oauth_cleared = !oauth_keys.is_empty();
        if oauth_cleared {
            // Delete the well-known OAuth DB credential keys for each OAuth key name.
            const CODEX_OAUTH_DB_KEY: &str = "__OAUTH_CHATGPT_CODEX";
            const COPILOT_OAUTH_DB_KEY: &str = "__OAUTH_GITHUB_COPILOT";
            const GITHUB_APP_OAUTH_DB_KEY: &str = "__OAUTH_GITHUB_APP";
            const GITHUB_INSTALLATION_ID_KEY: &str = "__GITHUB_INSTALLATION_ID";
            for key in &oauth_keys {
                let db_key = match key.as_str() {
                    "CHATGPT_CODEX_TOKEN" => CODEX_OAUTH_DB_KEY,
                    "GITHUB_COPILOT_TOKEN" => COPILOT_OAUTH_DB_KEY,
                    "GITHUB_APP_TOKEN" => {
                        // Also remove the installation ID when clearing GitHub App tokens.
                        let _ = credential_repo.delete(GITHUB_INSTALLATION_ID_KEY).await;
                        GITHUB_APP_OAUTH_DB_KEY
                    }
                    _ => continue,
                };
                let _ = credential_repo.delete(db_key).await;
            }
        }

        // 3. Delete custom provider entry (no-op for built-in providers).
        let custom_repo =
            CustomProviderRepository::new(self.state.db().clone(), self.state.event_bus());
        let custom_provider_deleted = match custom_repo.delete(provider_id).await {
            Ok(deleted) => deleted,
            Err(e) => {
                tracing::warn!(provider_id = %provider_id, error = %e, "provider_remove: custom provider delete failed");
                false
            }
        };

        // 4. Remove from in-memory catalog (custom providers only).
        if custom_provider_deleted {
            self.state.catalog().remove_custom_provider(provider_id);
        }

        tracing::info!(
            provider_id = %provider_id,
            credentials_deleted,
            custom_provider_deleted,
            oauth_cleared,
            "provider removed"
        );

        Json(ProviderRemoveResponse {
            ok: true,
            success: true,
            provider_id: input.provider_id,
            credentials_deleted,
            custom_provider_deleted,
            oauth_cleared,
            error: None,
        })
    }
}
