use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::settings::SettingsRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::AnyJson;
use crate::models::settings::DjinnSettings;

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
    pub value: AnyJson,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SettingsSetParams {
    /// Typed settings object. Unknown fields are rejected.
    pub settings: DjinnSettings,
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
        let repo = SettingsRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.get(&key).await {
            Ok(Some(setting)) => {
                let value = if key == SETTINGS_RAW_KEY {
                    // Deserialize through DjinnSettings so the response is always
                    // the canonical typed shape, even if the DB contains legacy JSON.
                    let typed = DjinnSettings::from_db_value(&setting.value);
                    serde_json::to_value(&typed).unwrap_or(serde_json::Value::Null)
                } else {
                    serde_json::json!(setting.value)
                };
                Json(SettingsGetResponse {
                    key,
                    exists: true,
                    value: AnyJson::from(value),
                })
            }
            Ok(None) => Json(SettingsGetResponse {
                key,
                exists: false,
                value: AnyJson::from(serde_json::Value::Null),
            }),
            Err(e) => Json(SettingsGetResponse {
                key,
                exists: false,
                value: AnyJson::from(serde_json::json!({ "error": e.to_string() })),
            }),
        }
    }

    #[tool(description = "Set and apply runtime server settings from raw JSON payload")]
    pub async fn settings_set(
        &self,
        Parameters(p): Parameters<SettingsSetParams>,
    ) -> Json<SettingsSetResponse> {
        match self.state.apply_settings(&p.settings).await {
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
        let repo = SettingsRepository::new(self.state.db().clone(), self.state.events().clone());
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
