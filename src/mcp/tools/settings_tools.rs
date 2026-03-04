use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::settings::SettingsRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::AnyJson;

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
    /// Raw JSON settings payload.
    pub raw: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SettingsSetResponse {
    pub ok: bool,
    pub applied: bool,
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SettingsReloadParams {}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SettingsReloadResponse {
    pub ok: bool,
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
                    serde_json::from_str::<serde_json::Value>(&setting.value)
                        .unwrap_or_else(|_| serde_json::json!({ "raw": setting.value }))
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
        match self.state.apply_settings_raw(&p.raw).await {
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

    #[tool(description = "Reload runtime settings from ~/.djinn/settings.json")]
    pub async fn settings_reload(
        &self,
        Parameters(_): Parameters<SettingsReloadParams>,
    ) -> Json<SettingsReloadResponse> {
        match self.state.reload_settings_from_disk().await {
            Ok(()) => Json(SettingsReloadResponse {
                ok: true,
                error: None,
            }),
            Err(e) => Json(SettingsReloadResponse {
                ok: false,
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
