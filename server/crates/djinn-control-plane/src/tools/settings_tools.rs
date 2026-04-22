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
    /// Ordered list of models available to all agents (e.g. ["openai/gpt-4o"]). Omit to keep current value.
    pub models: Option<Vec<String>>,
    /// Per-model concurrent session caps (e.g. {"openai/gpt-4o": 4}). Omit to keep current value.
    #[schemars(with = "Option<HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Enable the Linux-only ADR-057 memory FUSE mount for filesystem-first note workflows. Disabled by default; requires a Linux build with the `memory-mount` cargo feature. The mounted path serves the current session-selected task/worktree view when available and otherwise falls back to the canonical `main` view.
    pub memory_mount_enabled: Option<bool>,
    /// Absolute path for the Linux memory mount. The directory must already exist and be empty at startup. This path hosts the current session-selected memory view; no additional branch directories are exposed in this slice.
    pub memory_mount_path: Option<String>,
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
        if let Some(v) = p.max_sessions {
            settings.max_sessions = Some(v);
        }
        if let Some(v) = p.memory_mount_enabled {
            settings.memory_mount_enabled = Some(v);
        }
        if let Some(v) = p.memory_mount_path {
            settings.memory_mount_path = if v.is_empty() { None } else { Some(v) };
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
