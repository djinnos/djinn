use std::collections::HashMap;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use djinn_core::models::DjinnSettings;
use djinn_db::SettingsRepository;

const SETTINGS_RAW_KEY: &str = "settings.raw";

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SettingsGetParams {
    /// Optional settings key to fetch (defaults to settings.raw).
    pub key: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SettingsGetResponse {
    pub key: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "DjinnSettings")]
    pub settings: Option<DjinnSettings>,
    pub raw_value: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SettingsSetParams {
    /// Maximum number of tasks to dispatch per cycle. Omit to keep current value.
    #[schemars(with = "Option<i64>")]
    pub dispatch_limit: Option<u32>,
    /// Ordered model list for the 'worker' role (e.g. ["chatgpt_codex/gpt-5.3-codex"]). Omit to keep current value.
    pub model_priority_worker: Option<Vec<String>>,
    /// Ordered model list for the 'reviewer' role. Omit to keep current value.
    pub model_priority_reviewer: Option<Vec<String>>,
    /// Ordered model list for the 'lead' role. Omit to keep current value.
    pub model_priority_lead: Option<Vec<String>>,
    /// Ordered model list for the 'planner' role. Omit to keep current value.
    pub model_priority_planner: Option<Vec<String>>,
    /// Per-model concurrent session caps (e.g. {"chatgpt_codex/gpt-5.3-codex": 4}). Omit to keep current value.
    #[schemars(with = "Option<HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Model used for memory operations (knowledge extraction, summarisation). Format: "provider/model". Set to "" to clear. Omit to keep current value.
    pub memory_model: Option<String>,
    /// Langfuse public key for LLM observability (e.g. "pk-lf-..."). Set to "" to disable. Omit to keep current value.
    pub langfuse_public_key: Option<String>,
    /// Langfuse secret key for LLM observability (e.g. "sk-lf-..."). Set to "" to disable. Omit to keep current value.
    pub langfuse_secret_key: Option<String>,
    /// Langfuse OTLP endpoint URL (defaults to "http://localhost:3000/api/public/otel"). Set to "" to disable. Omit to keep current value.
    pub langfuse_endpoint: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SettingsSetResponse {
    pub ok: bool,
    pub applied: bool,
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SettingsResetParams {}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SettingsResetResponse {
    pub ok: bool,
    pub deleted: bool,
    pub error: Option<String>,
}

#[tool_router(router = settings_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(description = "Get persisted server settings value by key (defaults to settings.raw)")]
    pub async fn settings_get(
        &self,
        Parameters(p): Parameters<SettingsGetParams>,
    ) -> Json<SettingsGetResponse> {
        let key = p.key.unwrap_or_else(|| SETTINGS_RAW_KEY.to_string());
        let repo = SettingsRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.get(&key).await {
            Ok(Some(setting)) => {
                if key == SETTINGS_RAW_KEY {
                    // Deserialize through DjinnSettings so the response is always
                    // the canonical typed shape, even if the DB contains legacy JSON.
                    let typed = DjinnSettings::from_db_value(&setting.value);
                    Json(SettingsGetResponse {
                        key,
                        exists: true,
                        settings: Some(typed),
                        raw_value: None,
                        error: None,
                    })
                } else {
                    Json(SettingsGetResponse {
                        key,
                        exists: true,
                        settings: None,
                        raw_value: Some(setting.value),
                        error: None,
                    })
                }
            }
            Ok(None) => Json(SettingsGetResponse {
                key,
                exists: false,
                settings: None,
                raw_value: None,
                error: None,
            }),
            Err(e) => Json(SettingsGetResponse {
                key,
                exists: false,
                settings: None,
                raw_value: None,
                error: Some(e.to_string()),
            }),
        }
    }

    #[tool(
        description = "Patch runtime server settings. Only provided fields are updated; omitted fields keep their current values. Use individual model_priority_* params to set per-role model lists without overwriting other roles."
    )]
    pub async fn settings_set(
        &self,
        Parameters(p): Parameters<SettingsSetParams>,
    ) -> Json<SettingsSetResponse> {
        // Load existing settings so we can patch rather than replace.
        let repo = SettingsRepository::new(self.state.db().clone(), self.state.event_bus());
        let mut settings = match repo.get(SETTINGS_RAW_KEY).await {
            Ok(Some(s)) => DjinnSettings::from_db_value(&s.value),
            _ => DjinnSettings::default(),
        };

        if let Some(v) = p.dispatch_limit {
            settings.dispatch_limit = Some(v);
        }
        if let Some(v) = p.max_sessions {
            settings.max_sessions = Some(v);
        }
        if let Some(v) = p.model_priority_worker {
            settings
                .model_priority
                .get_or_insert_with(HashMap::new)
                .insert("worker".to_string(), v);
        }
        if let Some(v) = p.model_priority_reviewer {
            settings
                .model_priority
                .get_or_insert_with(HashMap::new)
                .insert("reviewer".to_string(), v);
        }
        if let Some(v) = p.model_priority_lead {
            settings
                .model_priority
                .get_or_insert_with(HashMap::new)
                .insert("lead".to_string(), v);
        }
        if let Some(v) = p.model_priority_planner {
            settings
                .model_priority
                .get_or_insert_with(HashMap::new)
                .insert("planner".to_string(), v);
        }
        if let Some(v) = p.memory_model {
            settings.memory_model = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = p.langfuse_public_key {
            settings.langfuse_public_key = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = p.langfuse_secret_key {
            settings.langfuse_secret_key = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = p.langfuse_endpoint {
            settings.langfuse_endpoint = if v.is_empty() { None } else { Some(v) };
        }

        match self.state.apply_settings(&settings).await {
            Ok(()) => Json(SettingsSetResponse {
                ok: true,
                applied: true,
                error: None,
            }),
            Err(e) => Json(SettingsSetResponse {
                ok: false,
                applied: false,
                error: Some(e),
            }),
        }
    }

    #[tool(description = "Reset runtime settings to defaults and clear persisted settings.raw")]
    pub async fn settings_reset(
        &self,
        Parameters(_): Parameters<SettingsResetParams>,
    ) -> Json<SettingsResetResponse> {
        let repo = SettingsRepository::new(self.state.db().clone(), self.state.event_bus());
        let deleted = match repo.delete(SETTINGS_RAW_KEY).await {
            Ok(v) => v,
            Err(e) => {
                return Json(SettingsResetResponse {
                    ok: false,
                    deleted: false,
                    error: Some(e.to_string()),
                });
            }
        };
        self.state.reset_runtime_settings().await;
        Json(SettingsResetResponse {
            ok: true,
            deleted,
            error: None,
        })
    }
}
