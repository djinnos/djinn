use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::db::repositories::custom_provider::CustomProviderRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::AnyJson;
use crate::models::provider::{CustomProvider, Model, Provider, SeedModel};
use crate::provider::validate::{self, ValidationRequest};

// ── Shared response helpers ───────────────────────────────────────────────────

fn provider_to_json(p: &Provider) -> serde_json::Value {
    serde_json::json!({
        "id":                   p.id,
        "name":                 p.name,
        "npm":                  p.npm,
        "env_vars":             p.env_vars,
        "base_url":             p.base_url,
        "docs_url":             p.docs_url,
        "is_openai_compatible": p.is_openai_compatible,
        // Always false from the server — desktop merges its local credential state.
        "connected": false,
    })
}

fn model_to_json(m: &Model) -> serde_json::Value {
    serde_json::json!({
        "id":             m.id,
        "provider_id":    m.provider_id,
        "name":           m.name,
        "tool_call":      m.tool_call,
        "reasoning":      m.reasoning,
        "attachment":     m.attachment,
        "context_window": m.context_window,
        "output_limit":   m.output_limit,
        "pricing": {
            "input_per_million":       m.pricing.input_per_million,
            "output_per_million":      m.pricing.output_per_million,
            "cache_read_per_million":  m.pricing.cache_read_per_million,
            "cache_write_per_million": m.pricing.cache_write_per_million,
        }
    })
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
    pub models: Vec<AnyJson>,
}

// ── provider_catalog ──────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProviderCatalogResponse {
    pub providers: Vec<AnyJson>,
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
    pub models: Vec<AnyJson>,
    pub total: i64,
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
    pub model: AnyJson,
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
                let models: Vec<AnyJson> = all
                    .iter()
                    .map(|h| AnyJson::from(serde_json::to_value(h).unwrap_or_default()))
                    .collect();
                Json(ModelHealthResponse {
                    action: "status".into(),
                    models,
                })
            }
            "reset" => {
                if let Some(model_id) = &input.model {
                    tracker.reset(model_id);
                    let h = tracker.model_health(model_id);
                    Json(ModelHealthResponse {
                        action: "reset".into(),
                        models: vec![AnyJson::from(serde_json::to_value(&h).unwrap_or_default())],
                    })
                } else {
                    Json(ModelHealthResponse {
                        action: "reset".into(),
                        models: vec![
                            AnyJson::from(serde_json::json!({"error": "model parameter required for reset"})),
                        ],
                    })
                }
            }
            "reset_all" => {
                tracker.reset_all();
                Json(ModelHealthResponse {
                    action: "reset_all".into(),
                    models: vec![],
                })
            }
            "enable" => {
                if let Some(model_id) = &input.model {
                    tracker.enable(model_id);
                    let h = tracker.model_health(model_id);
                    Json(ModelHealthResponse {
                        action: "enable".into(),
                        models: vec![AnyJson::from(serde_json::to_value(&h).unwrap_or_default())],
                    })
                } else {
                    Json(ModelHealthResponse {
                        action: "enable".into(),
                        models: vec![
                            AnyJson::from(serde_json::json!({"error": "model parameter required for enable"})),
                        ],
                    })
                }
            }
            _ => Json(ModelHealthResponse {
                action: action.to_owned(),
                models: vec![
                    AnyJson::from(serde_json::json!({"error": format!("unknown action '{action}'; valid: status, reset, reset_all, enable")})),
                ],
            }),
        }
    }

    /// List all LLM providers from the models.dev catalog. Each entry includes connection
    /// metadata (env vars, base URL, OpenAI-compat flag) and a connected placeholder for
    /// the desktop to merge local credential state.
    #[tool(
        description = "List all LLM providers from the models.dev catalog. Each entry includes connection metadata (env vars, base URL, OpenAI-compat flag) and a connected placeholder for the desktop to merge local credential state."
    )]
    pub async fn provider_catalog(&self) -> Json<ProviderCatalogResponse> {
        let providers: Vec<AnyJson> = self
            .state
            .catalog()
            .list_providers()
            .iter()
            .map(|p| AnyJson::from(provider_to_json(p)))
            .collect();
        let total = i64::try_from(providers.len()).unwrap_or(i64::MAX);
        Json(ProviderCatalogResponse { providers, total })
    }

    /// List all models for a provider. Each model includes capabilities
    /// (tool_call, reasoning, attachment), context limits, and per-million-token pricing.
    #[tool(
        description = "List all models for a provider. Each model includes capabilities (tool_call, reasoning, attachment), context limits, and per-million-token pricing."
    )]
    pub async fn provider_models(
        &self,
        Parameters(input): Parameters<ProviderModelsInput>,
    ) -> Json<ProviderModelsResponse> {
        let models: Vec<AnyJson> = self
            .state
            .catalog()
            .list_models(&input.provider_id)
            .iter()
            .map(|m| AnyJson::from(model_to_json(m)))
            .collect();
        let total = i64::try_from(models.len()).unwrap_or(i64::MAX);
        Json(ProviderModelsResponse {
            provider_id: input.provider_id,
            models,
            total,
        })
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
                model: AnyJson::from(model_to_json(&m)),
            }),
            None => Json(ProviderModelLookupResponse {
                model_id,
                found: false,
                model: AnyJson::from(serde_json::Value::Null),
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
                success: false,
                id: input.id,
                error: Some("provider id must be non-empty and must not contain '/'".into()),
            });
        }

        // Validate base_url scheme.
        if !input.base_url.starts_with("http://") && !input.base_url.starts_with("https://") {
            return Json(ProviderAddCustomResponse {
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
            success: true,
            id: input.id,
            error: None,
        })
    }
}
