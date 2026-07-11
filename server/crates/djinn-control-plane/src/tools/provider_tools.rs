// djinn:allow-oversize — provider_* MCP tools (catalog/connected/models/oauth/
// validate/remove + shared response shaping) cohere as one surface and already
// sat at the 50 KiB guideline before the slice-5 org-policy block filter pushed
// it just over. Splitting the module would scatter the shared helpers for no
// real readability gain; the file stays a single well-factored unit.
use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::server::DjinnMcpServer;
use crate::tools::acting_user;
use crate::tools::tool_error::{ErrorClass, ToolError};
use djinn_core::models::{Model, OrgAiPolicy, Provider};
use djinn_provider::catalog::builtin;
use djinn_provider::catalog::health::ModelHealth;
use djinn_provider::catalog::validate::{self, ValidationRequest};
use djinn_provider::repos::CredentialRepository;
use djinn_provider::repos::CustomProviderRepository;

// ── Shared response helpers ───────────────────────────────────────────────────

/// True when `provider_id` is a subscription that org policy has blocked.
/// Member-facing catalog/connected/model surfaces use this to hide blocked
/// subscriptions entirely. Admin API keys (non-subscription providers) are
/// never governed by the allowlist, so a non-subscription id is never blocked
/// even if (defensively) it appears in the set.
fn is_blocked_subscription(provider_id: &str, blocked: &HashSet<String>) -> bool {
    if blocked.is_empty() {
        return false;
    }
    builtin::is_subscription_provider(provider_id)
        && blocked.contains(&provider_id.to_ascii_lowercase())
}

/// Compute the effective `recommended` flag for a model using the org policy
/// override lists plus the built-in baseline. Priority: demotion wins over
/// addition; addition wins over the `builtin::is_recommended_model` baseline.
/// `surfaced_model_id` is the fully qualified `provider/model-id` as it will
/// appear in the API output (after any merged-child re-namespacing).
fn effective_recommended(provider_id: &str, surfaced_model_id: &str, policy: &OrgAiPolicy) -> bool {
    if policy
        .demoted_recommended_model_ids
        .iter()
        .any(|id| id == surfaced_model_id)
    {
        return false;
    }
    if policy
        .additional_recommended_model_ids
        .iter()
        .any(|id| id == surfaced_model_id)
    {
        return true;
    }
    builtin::is_recommended_model(provider_id, surfaced_model_id)
}

fn model_to_output(m: &Model) -> ProviderModelOutput {
    // Always return the full "provider/model" form for API consumers, where
    // the first path segment is the provider id and the remainder is the model
    // id (which may itself contain slashes, e.g. Fireworks'
    // "accounts/fireworks/models/kimi-k2p6" or OpenRouter's "vendor/model").
    // Only skip prefixing when the id is *already* qualified with this
    // provider's id — testing for a bare `contains('/')` would mistake a
    // multi-segment model path for an already-qualified id and drop the
    // provider, producing an unparseable reference at dispatch time.
    let full_id = if m.id.starts_with(&format!("{}/", m.provider_id)) {
        m.id.clone()
    } else {
        format!("{}/{}", m.provider_id, m.id)
    };
    let recommended = builtin::is_recommended_model(&m.provider_id, &m.id);
    ProviderModelOutput {
        id: full_id,
        provider_id: m.provider_id.clone(),
        name: m.name.clone(),
        tool_call: m.tool_call,
        reasoning: m.reasoning,
        attachment: m.attachment,
        context_window: m.context_window,
        output_limit: m.output_limit,
        recommended,
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

/// The revoked reason for `provider_id`, if its stored credential (or its merged
/// OAuth child, e.g. `openai` ← `chatgpt_codex`) was marked revoked. `revoked`
/// maps credential `provider_id` → reason (from
/// `CredentialRepository::list_revoked_for_user`).
fn revoked_reason_for(
    provider_id: &str,
    revoked: &std::collections::HashMap<String, String>,
) -> Option<String> {
    revoked
        .get(provider_id)
        .or_else(|| {
            builtin::resolve_oauth_provider(provider_id).and_then(|child| revoked.get(child))
        })
        .cloned()
}

fn is_provider_usable(provider: &Provider, builtin_ids: &HashSet<String>) -> bool {
    (provider.is_openai_compatible || builtin_ids.contains(&provider.id))
        && !builtin::is_auth_only_provider(&provider.id)
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
    /// Owning user the breaker bucket is scoped to; `null` = shared/system
    /// bucket (org-shared credential). Health is tracked per `(scope, model)`,
    /// so the same model can appear once per user that has used it.
    pub scope: Option<String>,
    pub auto_disabled: bool,
    #[schemars(with = "i64")]
    pub consecutive_failures: u32,
    #[schemars(with = "i64")]
    pub total_failures: u32,
    #[schemars(with = "i64")]
    pub total_successes: u32,
    /// Current escalating-cooldown tier (how many trips since the last success).
    /// Each tier triples the auto-disable cooldown (5s → 15s → … → 4h cap).
    #[schemars(with = "i64")]
    pub disable_ttl_trips: u32,
    #[schemars(with = "Option<i64>")]
    pub cooldown_seconds_remaining: Option<u64>,
    /// Hard-disabled by the trip-rate ceiling: held unavailable with no
    /// auto-expiry until a human re-enables it via `model_health(action=enable)`.
    /// When true, `cooldown_seconds_remaining` is null (there is no auto-expiry).
    pub hard_disabled: bool,
    /// Number of breaker trips inside the rolling trip-rate window (6h). When it
    /// reaches the ceiling (8) the bucket hard-disables.
    #[schemars(with = "i64")]
    pub trips_in_window: u32,
}

impl From<ModelHealth> for ModelHealthOutput {
    fn from(value: ModelHealth) -> Self {
        Self {
            model_id: value.model_id,
            scope: value.scope,
            auto_disabled: value.auto_disabled,
            consecutive_failures: value.consecutive_failures,
            total_failures: value.total_failures,
            total_successes: value.total_successes,
            disable_ttl_trips: value.disable_ttl_trips,
            cooldown_seconds_remaining: value.cooldown_seconds_remaining,
            hard_disabled: value.hard_disabled,
            trips_in_window: value.trips_in_window,
        }
    }
}

// ── provider_catalog ──────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Default)]
pub struct ProviderCatalogInput {
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
}

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
    /// When set, the stored credential for this provider was rejected by the
    /// provider (a 401 during a run) and marked revoked. The provider is
    /// reported disconnected (`connected = false`, no `connection_methods`) and
    /// this human-readable reason is carried so the UI can show
    /// "Disconnected — <reason>" persistently (survives reload — it comes from
    /// the persisted `credentials.revoked_at/reason`, not a transient event).
    /// Reconnecting the provider clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_reason: Option<String>,
}

// ── provider_connected ────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Default)]
pub struct ProviderConnectedInput {
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
}

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
    /// Whether this is a curated flagship the UI should surface as
    /// "recommended" (latest state-of-the-art model for its provider). Set from
    /// the server-side flagship map (`builtin::is_recommended_model`); a provider
    /// with no curated recommendation has this `false` on every model and the UI
    /// falls back to showing all of them.
    pub recommended: bool,
    pub pricing: ModelPricingOutput,
}

// ── provider_models_connected ─────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema, Default)]
pub struct ProviderModelsConnectedInput {
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
}

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
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
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
    /// Device-code: the short code the user must enter at `verification_uri`.
    pub user_code: Option<String>,
    /// Device-code: the bare URL the user opens to enter the code manually.
    pub verification_uri: Option<String>,
    /// Device-code: convenience URL with `user_code` pre-filled as a query
    /// param; the UI can surface this as a "click to open" link.
    pub verification_uri_complete: Option<String>,
    /// Device-code: recommended polling interval in seconds (informational —
    /// the server does the polling, not the client).
    pub interval: Option<i64>,
    /// Device-code: how long, in seconds, the user has to complete sign-in.
    pub expires_in: Option<i64>,
    /// True when the flow is still in progress — the server has spawned a
    /// background task that polls for tokens. The UI should wait for a
    /// `credential.updated` SSE event to confirm completion.
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
    /// G3 structured-error envelope, populated on a 404-style miss (the model id
    /// is not in the catalog). Lets the agent branch on `status == "404"` and
    /// follow the `hint` instead of re-guessing the same bad id. Absent on a hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
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
                    // Wipe the breaker state for this model across every user
                    // scope ("reset gpt-5.5 for everyone").
                    tracker.reset_model_all_scopes(model_id);
                    self.state.persist_model_health_state().await;
                    Json(ModelHealthResponse {
                        action: "reset".into(),
                        models: vec![ModelHealthOutput::from(
                            tracker.model_health(None, model_id),
                        )],
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
                    // Re-enable this model for every user scope that has it
                    // disabled ("let everyone use gpt-5.5 again now"), keeping
                    // failure counters intact.
                    tracker.enable_model_all_scopes(model_id);
                    self.state.persist_model_health_state().await;
                    let models: Vec<ModelHealthOutput> = tracker
                        .all_health()
                        .into_iter()
                        .filter(|h| &h.model_id == model_id)
                        .map(ModelHealthOutput::from)
                        .collect();
                    Json(ModelHealthResponse {
                        action: "enable".into(),
                        models,
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
    pub async fn provider_catalog(
        &self,
        Parameters(input): Parameters<ProviderCatalogInput>,
    ) -> Json<ProviderCatalogResponse> {
        let builtin_ids = builtin::builtin_provider_ids();
        let merged_ids = builtin::merged_provider_ids();
        let effective_user = match acting_user::resolve_effective_user(
            self.state.db(),
            input.target_user_id.as_deref(),
        )
        .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "provider_catalog: act-as denied");
                return Json(ProviderCatalogResponse {
                    providers: vec![],
                    total: 0,
                });
            }
        };
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());
        let (credential_provider_ids, credential_key_names) = match credential_repo
            .list_for_user(effective_user.as_deref())
            .await
        {
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

        let revoked_map: std::collections::HashMap<String, String> = credential_repo
            .list_revoked_for_user(effective_user.as_deref())
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        // Org policy: hide subscriptions an admin has blocked so members never
        // see (or can connect) them.
        let blocked_subscriptions = self.state.blocked_subscription_ids().await;

        let providers: Vec<ProviderCatalogItem> = self
            .state
            .catalog()
            .list_providers()
            .iter()
            .filter(|p| is_provider_usable(p, &builtin_ids))
            // Hide providers that are merged into a parent (e.g. chatgpt_codex → openai).
            .filter(|p| !merged_ids.contains(&p.id))
            // Hide org-policy-blocked subscriptions entirely.
            .filter(|p| !is_blocked_subscription(&p.id, &blocked_subscriptions))
            .map(|p| {
                let oauth_keys = builtin::all_oauth_keys_for_provider(&p.id);
                let (connected, methods) = provider_connection_status(
                    p,
                    &oauth_keys,
                    &credential_provider_ids,
                    &credential_key_names,
                );
                // A revoked credential overrides "connected": report the provider
                // disconnected (no methods) + the reason, so the UI prompts a
                // reconnect persistently.
                let revoked_reason = revoked_reason_for(&p.id, &revoked_map);
                let connected = connected && revoked_reason.is_none();
                let methods: Vec<String> = if revoked_reason.is_some() {
                    Vec::new()
                } else {
                    methods.into_iter().map(str::to_string).collect()
                };
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
                    connection_methods: methods,
                    revoked_reason,
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
    pub async fn provider_connected(
        &self,
        Parameters(input): Parameters<ProviderConnectedInput>,
    ) -> Json<ProviderConnectedResponse> {
        let builtin_ids = builtin::builtin_provider_ids();
        let merged_ids = builtin::merged_provider_ids();
        let effective_user = match acting_user::resolve_effective_user(
            self.state.db(),
            input.target_user_id.as_deref(),
        )
        .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "provider_connected: act-as denied");
                return Json(ProviderConnectedResponse {
                    providers: vec![],
                    total: 0,
                });
            }
        };
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());
        let (credential_provider_ids, credential_key_names) = match credential_repo
            .list_for_user(effective_user.as_deref())
            .await
        {
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

        let revoked_map: std::collections::HashMap<String, String> = credential_repo
            .list_revoked_for_user(effective_user.as_deref())
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        let blocked_subscriptions = self.state.blocked_subscription_ids().await;

        let providers: Vec<ProviderCatalogItem> = self
            .state
            .catalog()
            .list_providers()
            .iter()
            .filter(|p| is_provider_usable(p, &builtin_ids))
            .filter(|p| !merged_ids.contains(&p.id))
            // Hide org-policy-blocked subscriptions entirely.
            .filter(|p| !is_blocked_subscription(&p.id, &blocked_subscriptions))
            .filter_map(|p| {
                let oauth_keys = builtin::all_oauth_keys_for_provider(&p.id);
                let (connected, methods) = provider_connection_status(
                    p,
                    &oauth_keys,
                    &credential_provider_ids,
                    &credential_key_names,
                );
                // A revoked credential is NOT connected — exclude it from the
                // connected-only list.
                if !connected || revoked_reason_for(&p.id, &revoked_map).is_some() {
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
                    revoked_reason: None,
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
    pub async fn provider_models_connected(
        &self,
        Parameters(input): Parameters<ProviderModelsConnectedInput>,
    ) -> Json<ProviderModelsConnectedResponse> {
        let builtin_ids = builtin::builtin_provider_ids();
        let merged_ids = builtin::merged_provider_ids();
        let effective_user = match acting_user::resolve_effective_user(
            self.state.db(),
            input.target_user_id.as_deref(),
        )
        .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "provider_models_connected: act-as denied");
                return Json(ProviderModelsConnectedResponse {
                    models: vec![],
                    total: 0,
                });
            }
        };
        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());
        let credentials = credential_repo
            .list_for_user(effective_user.as_deref())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "provider_models_connected: failed to load credentials");
                Vec::new()
            });
        let connected_set = self.state.catalog().connected_provider_ids(&credentials);
        let blocked_subscriptions = self.state.blocked_subscription_ids().await;

        // Load org policy for recommended-model overrides once.
        let org_policy = self.state.org_ai_policy().await;

        // Log a warning for any legacy/corrupt overlap between additions and
        // demotions. Demotion wins at runtime (effective_recommended checks
        // demotions first), but persisted data should be clean.
        {
            let additional_set: HashSet<String> = org_policy
                .additional_recommended_model_ids
                .iter()
                .cloned()
                .collect();
            let demoted_set: HashSet<String> = org_policy
                .demoted_recommended_model_ids
                .iter()
                .cloned()
                .collect();
            let overlap: Vec<&String> = additional_set.intersection(&demoted_set).collect();
            if !overlap.is_empty() {
                tracing::warn!(
                    overlap = ?overlap,
                    "org_ai_policy: same model id(s) appear in both \
                     additional_recommended_model_ids and \
                     demoted_recommended_model_ids; treating as demoted"
                );
            }
        }

        // Collect connected provider IDs including merged children. Org-policy-
        // blocked subscriptions (parent or merged child) are skipped so their
        // models never reach a member.
        let mut connected_provider_ids: Vec<String> = Vec::new();
        for p in self.state.catalog().list_providers().iter() {
            if !is_provider_usable(p, &builtin_ids) || !connected_set.contains(&p.id) {
                continue;
            }
            if is_blocked_subscription(&p.id, &blocked_subscriptions) {
                continue;
            }
            connected_provider_ids.push(p.id.clone());
            // If this parent has merged children, include their models too.
            if !merged_ids.is_empty() {
                for child_id in &merged_ids {
                    if builtin::find_builtin_provider(child_id).and_then(|bp| bp.merge_into)
                        == Some(p.id.as_str())
                        && !is_blocked_subscription(child_id, &blocked_subscriptions)
                    {
                        connected_provider_ids.push(child_id.clone());
                    }
                }
            }
        }

        let org_policy_ref = &org_policy;
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
                        // Only merged children need re-namespacing (child →
                        // parent). For everything else `display_pid == pid` and
                        // we must leave the id untouched — re-tagging here used
                        // to split on the first '/', which silently dropped the
                        // leading segment of multi-segment model paths (e.g.
                        // Fireworks' "accounts/..."), corrupting the id.
                        if display_pid != *pid {
                            out.provider_id = display_pid.clone();
                            // Strip the original provider prefix, keep the rest
                            // (model id, slashes and all), re-prefix with parent.
                            let rest = out
                                .id
                                .strip_prefix(&format!("{pid}/"))
                                .unwrap_or(&out.id)
                                .to_string();
                            out.id = format!("{display_pid}/{rest}");
                        }
                        // Apply effective recommended policy (demotion →
                        // addition → builtin baseline) for every output row.
                        // For merged children this runs after re-namespacing so
                        // the surfaced id is used.
                        out.recommended =
                            effective_recommended(&out.provider_id, &out.id, org_policy_ref);
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

        fn failure(
            provider_id: String,
            builtin_id: Option<&str>,
            oauth_supported: bool,
            error: String,
        ) -> ProviderOauthStartResponse {
            ProviderOauthStartResponse {
                ok: false,
                success: false,
                provider_id,
                builtin_id: builtin_id.map(str::to_string),
                legacy_builtin_id: builtin_id.map(str::to_string),
                oauth_supported,
                configured_keys: vec![],
                error: Some(error),
                user_code: None,
                verification_uri: None,
                verification_uri_complete: None,
                interval: None,
                expires_in: None,
                pending: false,
            }
        }

        let resolved_name = builtin::resolve_builtin_name(&input.provider_id);
        let Some(builtin_id) = resolved_name else {
            return Json(failure(
                input.provider_id,
                None,
                false,
                "provider is not a known built-in".into(),
            ));
        };

        // Resolve the effective owner for the stored OAuth token. With no
        // `target_user_id` this is the acting user; an admin may target another
        // user (e.g. another target user that can't self-configure).
        let effective_user = match acting_user::resolve_effective_user(
            self.state.db(),
            input.target_user_id.as_deref(),
        )
        .await
        {
            Ok(u) => u,
            Err(e) => {
                return Json(failure(input.provider_id, Some(builtin_id), false, e));
            }
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
            return Json(failure(
                input.provider_id,
                Some(builtin_id),
                false,
                "provider does not support OAuth flow".into(),
            ));
        }

        let Some(flow_kind) = OAuthFlowKind::from_provider_id(effective_id) else {
            return Json(failure(
                input.provider_id,
                Some(builtin_id),
                true,
                format!("no OAuth flow implemented for '{effective_id}'"),
            ));
        };

        let credential_repo =
            CredentialRepository::new(self.state.db().clone(), self.state.event_bus());

        match flow_kind {
            OAuthFlowKind::Codex => {
                // Device-code flow: `start_codex_device_auth` hits OpenAI's
                // `/deviceauth/usercode` endpoint and spawns a background
                // polling task. The UI displays the user_code and waits for
                // the `credential.updated` SSE event to confirm sign-in.
                let events = self.state.event_bus();
                match codex::start_codex_device_auth(credential_repo, &events, effective_user).await
                {
                    Ok(None) => Json(ProviderOauthStartResponse {
                        // Already connected (cached token valid or silently refreshed).
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
                        verification_uri_complete: None,
                        interval: None,
                        expires_in: None,
                        pending: false,
                    }),
                    Ok(Some(session)) => Json(ProviderOauthStartResponse {
                        ok: true,
                        success: false,
                        provider_id: input.provider_id,
                        builtin_id: Some(builtin_id.to_string()),
                        legacy_builtin_id: Some(builtin_id.to_string()),
                        oauth_supported: true,
                        configured_keys: oauth_keys,
                        error: None,
                        user_code: Some(session.user_code),
                        verification_uri: Some(session.verification_uri),
                        verification_uri_complete: Some(session.verification_uri_complete),
                        interval: Some(session.interval),
                        expires_in: Some(session.expires_in),
                        pending: true,
                    }),
                    Err(e) => Json(failure(
                        input.provider_id,
                        Some(builtin_id),
                        true,
                        e.to_string(),
                    )),
                }
            }
            OAuthFlowKind::Copilot => {
                let result = match copilot::start_copilot_flow().await {
                    Ok(session) => copilot::poll_copilot_flow(session, &credential_repo)
                        .await
                        .map(|_| ()),
                    Err(e) => Err(e),
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
                        verification_uri_complete: None,
                        interval: None,
                        expires_in: None,
                        pending: false,
                    }),
                    Err(e) => Json(failure(
                        input.provider_id,
                        Some(builtin_id),
                        true,
                        e.to_string(),
                    )),
                }
            }
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
                error: None,
            }),
            None => {
                // 404-style miss: surface a structured envelope so the agent can
                // distinguish "this model id doesn't exist" from a transient
                // failure and act on the hint instead of retrying the same id.
                let error = ToolError::new(format!("model '{model_id}' not found in catalog"))
                    .with_http_status(404)
                    .with_error_class(ErrorClass::NotFound)
                    .with_method("provider_model_lookup")
                    .with_path(model_id.clone())
                    .with_hint(
                        "Verify the id is in 'providerID/modelID' form; call \
                         provider_models_connected to list valid, connected model ids.",
                    );
                Json(ProviderModelLookupResponse {
                    model_id,
                    found: false,
                    model: None,
                    error: Some(error),
                })
            }
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
            // Still cleaned up here so operators who "Remove" the GitHub
            // integration after upgrading also drop the retired device-code
            // credential row — it isn't written anymore, but existing rows
            // should be wiped when the user asks.
            const LEGACY_GITHUB_APP_OAUTH_DB_KEY: &str = "__OAUTH_GITHUB_APP";
            for key in &oauth_keys {
                let db_key = match key.as_str() {
                    "CHATGPT_CODEX_TOKEN" => CODEX_OAUTH_DB_KEY,
                    "GITHUB_COPILOT_TOKEN" => COPILOT_OAUTH_DB_KEY,
                    "GITHUB_APP_TOKEN" => LEGACY_GITHUB_APP_OAUTH_DB_KEY,
                    _ => continue,
                };
                let _ = credential_repo.delete(db_key).await;
            }
        }

        // 3. Delete custom provider entry (no-op for built-in providers).
        //
        // Steps 1–2 above are the *per-user* disconnect (the acting user's own
        // credentials + OAuth tokens) and stay open to everyone. Deleting the
        // custom-provider *definition*, however, is org-shared — it removes the
        // provider for ALL users — so it is admin-only. A non-admin "Remove"
        // therefore disconnects their own keys but leaves the shared definition
        // intact. The no-user trusted path is still allowed (local/background).
        let custom_repo =
            CustomProviderRepository::new(self.state.db().clone(), self.state.event_bus());
        let custom_provider_deleted = match acting_user::require_admin(self.state.db()).await {
            Ok(()) => match custom_repo.delete(provider_id).await {
                Ok(deleted) => deleted,
                Err(e) => {
                    tracing::warn!(provider_id = %provider_id, error = %e, "provider_remove: custom provider delete failed");
                    false
                }
            },
            Err(_) => {
                tracing::info!(provider_id = %provider_id, "provider_remove: non-admin removed own credentials; shared custom-provider definition left intact");
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

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::models::Pricing;

    fn model(id: &str, provider_id: &str) -> Model {
        Model {
            id: id.to_string(),
            provider_id: provider_id.to_string(),
            name: "Test Model".to_string(),
            tool_call: true,
            reasoning: false,
            attachment: false,
            context_window: 0,
            output_limit: 0,
            pricing: Pricing::default(),
        }
    }

    #[test]
    fn full_id_prefixes_bare_model_ids() {
        // Single-segment ids get the provider prepended.
        assert_eq!(
            model_to_output(&model("gpt-5.5", "openai")).id,
            "openai/gpt-5.5"
        );
    }

    #[test]
    fn full_id_preserves_multi_segment_model_paths() {
        // Regression: a models.dev id that is itself a slash-delimited path
        // (Fireworks) must keep every segment and gain the provider prefix —
        // not be mistaken for an already-qualified id and have its leading
        // segment treated as the provider.
        assert_eq!(
            model_to_output(&model(
                "accounts/fireworks/models/kimi-k2p6",
                "fireworks-ai"
            ))
            .id,
            "fireworks-ai/accounts/fireworks/models/kimi-k2p6"
        );
        // The recovered "provider/model" split must round-trip back to the
        // exact model id Fireworks expects.
        let full = model_to_output(&model(
            "accounts/fireworks/models/kimi-k2p6",
            "fireworks-ai",
        ))
        .id;
        let (provider, model_id) = full.split_once('/').unwrap();
        assert_eq!(provider, "fireworks-ai");
        assert_eq!(model_id, "accounts/fireworks/models/kimi-k2p6");
    }

    #[test]
    fn full_id_does_not_double_prefix_already_qualified_ids() {
        // Synthetic providers may already embed their own provider prefix.
        assert_eq!(
            model_to_output(&model("synthetic/GLM-4.7", "synthetic")).id,
            "synthetic/GLM-4.7"
        );
    }
}
