use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::require_admin;
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
    /// Ordered list of models available to all agents (e.g. ["openai/gpt-4o"]). Omit to keep current value.
    /// This is the deployment FALLBACK list (used for tasks with no creator and
    /// users with no per-user selection); per-user model selection + concurrency
    /// caps live in `user_settings_*`.
    pub models: Option<Vec<String>>,
    /// Maximum total injected knowledge size in UTF-8 bytes (256 through 32768).
    #[schemars(with = "Option<i64>")]
    pub knowledge_injection_budget_bytes: Option<u32>,
    /// Maximum injected knowledge summary size in UTF-8 bytes (128 through 4096).
    #[schemars(with = "Option<i64>")]
    pub knowledge_injection_line_cap_bytes: Option<u32>,
    /// Maximum retrieved knowledge candidates considered for injection (1 through 50).
    #[schemars(with = "Option<i64>")]
    pub knowledge_injection_limit: Option<u32>,
    /// Injection-starvation threshold in percent (1 through 100).
    #[schemars(with = "Option<i64>")]
    pub injection_starvation_threshold_percent: Option<u32>,
    /// Minimum retrieval queries for starvation evaluation (1 through 10000).
    #[schemars(with = "Option<i64>")]
    pub injection_starvation_query_floor: Option<u32>,
    /// Retrieval-health aggregation window in minutes (5 through 10080).
    #[schemars(with = "Option<i64>")]
    pub retrieval_health_window_minutes: Option<u32>,
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
        description = "Patch runtime server settings. Only provided fields are updated; omitted fields keep their current values."
    )]
    pub async fn settings_set(
        &self,
        Parameters(p): Parameters<SettingsSetParams>,
    ) -> Json<SettingsSetResponse> {
        if let Err(error) = require_admin(self.state.db()).await {
            return Json(SettingsSetResponse {
                ok: false,
                applied: false,
                error: Some(error),
            });
        }
        // Load existing settings so we can patch rather than replace.
        let repo = SettingsRepository::new(self.state.db().clone(), self.state.event_bus());
        let mut settings = match repo.get(SETTINGS_RAW_KEY).await {
            Ok(Some(s)) => DjinnSettings::from_db_value(&s.value),
            _ => DjinnSettings::default(),
        };

        if let Some(v) = p.dispatch_limit {
            settings.dispatch_limit = Some(v);
        }
        if let Some(v) = p.models {
            settings.models = Some(v);
        }
        if let Some(v) = p.knowledge_injection_budget_bytes {
            settings.knowledge_injection_budget_bytes = Some(v);
        }
        if let Some(v) = p.knowledge_injection_line_cap_bytes {
            settings.knowledge_injection_line_cap_bytes = Some(v);
        }
        if let Some(v) = p.knowledge_injection_limit {
            settings.knowledge_injection_limit = Some(v);
        }
        if let Some(v) = p.injection_starvation_threshold_percent {
            settings.injection_starvation_threshold_percent = Some(v);
        }
        if let Some(v) = p.injection_starvation_query_floor {
            settings.injection_starvation_query_floor = Some(v);
        }
        if let Some(v) = p.retrieval_health_window_minutes {
            settings.retrieval_health_window_minutes = Some(v);
        }
        if let Err(e) =
            djinn_core::models::KnowledgeInjectionConfig::from_settings_and_env(&settings)
        {
            return Json(SettingsSetResponse {
                ok: false,
                applied: false,
                error: Some(e.to_string()),
            });
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
        if let Err(error) = require_admin(self.state.db()).await {
            return Json(SettingsResetResponse {
                ok: false,
                deleted: false,
                error: Some(error),
            });
        }
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
