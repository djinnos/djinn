use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::lifecycle_ops::resolve_project;
use crate::server::DjinnMcpServer;
use crate::tools::ObjectJson;
use djinn_db::ProjectRepository;

/// Build a success-shape `ProjectConfigResponse` from a fully-populated
/// [`ProjectConfig`].
fn project_config_ok(
    project: &djinn_core::models::Project,
    config: djinn_db::ProjectConfig,
) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status: "ok".into(),
        project: project.slug(),
        target_branch: config.target_branch,
        auto_merge: config.auto_merge,
        sync_enabled: config.sync_enabled,
        sync_remote: config.sync_remote,
        graph_excluded_paths: config.graph_excluded_paths,
        graph_orphan_ignore: config.graph_orphan_ignore,
    }
}

/// Fallback shape used when `get_config` returns `None` (no row) or
/// an error — we still echo back the denormalized fields from the
/// `Project` row itself.
fn project_config_fallback(
    status: String,
    project: &djinn_core::models::Project,
) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status,
        project: project.slug(),
        target_branch: project.target_branch.clone(),
        auto_merge: project.auto_merge,
        sync_enabled: project.sync_enabled,
        sync_remote: project.sync_remote.clone(),
        graph_excluded_paths: Vec::new(),
        graph_orphan_ignore: Vec::new(),
    }
}

/// Error shape used when the project lookup itself fails, so we don't
/// even have a `Project` to echo.
fn project_config_error(project_ref: &str, status: String) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status,
        project: project_ref.to_owned(),
        target_branch: "main".into(),
        auto_merge: true,
        sync_enabled: false,
        sync_remote: None,
        graph_excluded_paths: Vec::new(),
        graph_orphan_ignore: Vec::new(),
    }
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProjectConfigGetParams {
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectConfigSetParams {
    pub project: String,
    pub key: String,
    pub value: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigGetParams {
    /// Project UUID.
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigSetParams {
    /// Project UUID.
    pub project: String,
    /// Full `EnvironmentConfig` JSON blob. Validated server-side via
    /// `djinn_stack::environment::EnvironmentConfig::validate` before
    /// anything is written.
    #[schemars(with = "djinn_stack::environment::EnvironmentConfig")]
    pub config: ObjectJson,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigResetParams {
    /// Project UUID.
    pub project: String,
}

// ── Response structs ─────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProjectConfigResponse {
    pub status: String,
    pub project: String,
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
    /// Glob patterns the `code_graph` MCP handler drops from
    /// cycles/orphans/ranked result sets (migration 12). Canonical empty
    /// value is an empty array, not null, so the UI can bind a list
    /// editor to it without a pre-fetch fallback.
    #[serde(default)]
    pub graph_excluded_paths: Vec<String>,
    /// Exact file paths the `code_graph orphans` op silently drops
    /// (migration 12). Intended for the Dead-code panel's "mark not
    /// actually dead" affordance.
    #[serde(default)]
    pub graph_orphan_ignore: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigGetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The raw JSON config currently in `projects.environment_config`.
    /// Empty object `{}` when the row hasn't been reseeded yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<djinn_stack::environment::EnvironmentConfig>")]
    pub config: Option<ObjectJson>,
    /// The catalog image this project is assigned to, if any (for the picker).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_image_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_image_name: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigSetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigResetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The freshly-generated auto-detected config, on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<djinn_stack::environment::EnvironmentConfig>")]
    pub config: Option<ObjectJson>,
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = config_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    #[tool(description = "Get project config fields for a project path.")]
    pub async fn project_config_get(
        &self,
        Parameters(input): Parameters<ProjectConfigGetParams>,
    ) -> Json<ProjectConfigResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match resolve_project(&repo, &input.project).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Json(project_config_error(
                    &input.project,
                    format!("error: project not found: {}", input.project),
                ));
            }
            Err(e) => {
                return Json(project_config_error(&input.project, format!("error: {e}")));
            }
        };
        match repo.get_config(&project.id).await {
            Ok(Some(config)) => Json(project_config_ok(&project, config)),
            Ok(None) => Json(project_config_fallback("ok".into(), &project)),
            Err(e) => Json(project_config_fallback(format!("error: {e}"), &project)),
        }
    }

    #[tool(description = "Set a single project config field by key.")]
    pub async fn project_config_set(
        &self,
        Parameters(input): Parameters<ProjectConfigSetParams>,
    ) -> Json<ProjectConfigResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match resolve_project(&repo, &input.project).await {
            Ok(Some(project)) => project,
            Ok(None) => {
                return Json(project_config_error(
                    &input.project,
                    format!("error: project not found: {}", input.project),
                ));
            }
            Err(e) => {
                return Json(project_config_error(&input.project, format!("error: {e}")));
            }
        };

        match repo
            .update_config_field(&project.id, &input.key, &input.value)
            .await
        {
            Ok(Some(config)) => Json(project_config_ok(&project, config)),
            Ok(None) => Json(project_config_fallback(
                format!("error: invalid key '{}'", input.key),
                &project,
            )),
            Err(e) => Json(project_config_fallback(format!("error: {e}"), &project)),
        }
    }

    /// Return the current `environment_config` JSON for a project.
    ///
    /// Returns `{}` while the boot reseed hook hasn't seen the row yet
    /// — callers can treat that as "show the auto-detection preview"
    /// or surface a "not seeded yet" state.
    #[tool(
        description = "Read projects.environment_config as JSON. Returns '{}' for projects that haven't been reseeded yet."
    )]
    pub async fn project_environment_config_get(
        &self,
        Parameters(input): Parameters<ProjectEnvironmentConfigGetParams>,
    ) -> Json<ProjectEnvironmentConfigGetResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.get_environment_config(&input.project).await {
            Ok(Some(raw)) => {
                let parsed = serde_json::from_str::<serde_json::Value>(&raw)
                    .unwrap_or(serde_json::json!({}));
                // Surface the assigned catalog image so the UI picker can
                // pre-select it by name.
                let selected = djinn_db::ImageRepository::new(self.state.db().clone())
                    .resolve_for_project(&input.project)
                    .await
                    .ok()
                    .flatten();
                Json(ProjectEnvironmentConfigGetResponse {
                    status: "ok".into(),
                    error: None,
                    config: Some(ObjectJson::from(parsed)),
                    selected_image_id: selected.as_ref().map(|i| i.id.clone()),
                    selected_image_name: selected.map(|i| i.name),
                })
            }
            Ok(None) => Json(ProjectEnvironmentConfigGetResponse {
                status: "error".into(),
                error: Some(format!("project not found: {}", input.project)),
                config: None,
                selected_image_id: None,
                selected_image_name: None,
            }),
            Err(err) => Json(ProjectEnvironmentConfigGetResponse {
                status: "error".into(),
                error: Some(format!("db error: {err}")),
                config: None,
                selected_image_id: None,
                selected_image_name: None,
            }),
        }
    }

    /// Write a validated `environment_config` JSON blob for a project.
    ///
    /// Flow: validate → upsert the runtime ConfigMap (so warm/task-run
    /// Pods scheduled after this call see the new config) → write to
    /// Dolt (which nulls `image_hash` so the next mirror-fetch tick
    /// rebuilds the image).
    #[tool(
        description = "Validate + persist projects.environment_config, upsert the runtime ConfigMap, and null image_hash so the next tick rebuilds the image. Accepts a JSON EnvironmentConfig."
    )]
    pub async fn project_environment_config_set(
        &self,
        Parameters(input): Parameters<ProjectEnvironmentConfigSetParams>,
    ) -> Json<ProjectEnvironmentConfigSetResponse> {
        // Parse + validate up front so the MCP error surface is the
        // typed EnvironmentConfigError, not whatever the DB layer
        // returns later.
        let cfg: djinn_stack::environment::EnvironmentConfig =
            match serde_json::from_value(serde_json::Value::Object(input.config.0)) {
                Ok(c) => c,
                Err(err) => {
                    return Json(ProjectEnvironmentConfigSetResponse {
                        status: "error".into(),
                        error: Some(format!("parse config: {err}")),
                    });
                }
            };
        if let Err(err) = cfg.validate() {
            return Json(ProjectEnvironmentConfigSetResponse {
                status: "error".into(),
                error: Some(format!("validate: {err}")),
            });
        }

        // Mark it as user-edited so the boot reseed hook leaves it
        // alone on the next server restart.
        let mut cfg = cfg;
        cfg.source = djinn_stack::environment::ConfigSource::UserEdited;

        // Dispatch through the RuntimeOps bridge — production apps
        // upsert the runtime ConfigMap via the image-controller; test
        // stubs fall back to a plain DB write.
        if let Err(err) = self
            .state
            .apply_environment_config(&input.project, &cfg)
            .await
        {
            return Json(ProjectEnvironmentConfigSetResponse {
                status: "error".into(),
                error: Some(format!("apply: {err}")),
            });
        }

        Json(ProjectEnvironmentConfigSetResponse {
            status: "ok".into(),
            error: None,
        })
    }

    /// Regenerate `environment_config` from the project's current `stack`
    /// column and persist it. Mirrors the boot reseed hook but runs on
    /// demand — the UI's "Reset from auto-detection" button calls this.
    /// The freshly-generated config is tagged `source: AutoDetected`,
    /// so the next boot reseed will still skip it (schema_version >= 1).
    #[tool(
        description = "Regenerate projects.environment_config from projects.stack, overwriting any user edits. Returns the freshly-generated config. Fails if the stack column is empty (no detection has run yet)."
    )]
    pub async fn project_environment_config_reset(
        &self,
        Parameters(input): Parameters<ProjectEnvironmentConfigResetParams>,
    ) -> Json<ProjectEnvironmentConfigResetResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        let stack_raw = match repo.get_stack(&input.project).await {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                return Json(ProjectEnvironmentConfigResetResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                    config: None,
                });
            }
            Err(err) => {
                return Json(ProjectEnvironmentConfigResetResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {err}")),
                    config: None,
                });
            }
        };
        let trimmed = stack_raw.trim();
        if trimmed.is_empty() || trimmed == "{}" {
            return Json(ProjectEnvironmentConfigResetResponse {
                status: "error".into(),
                error: Some(
                    "project has no detected stack yet — wait for the next mirror-fetch tick and retry"
                        .into(),
                ),
                config: None,
            });
        }
        let stack: djinn_stack::schema::Stack = match serde_json::from_str(trimmed) {
            Ok(s) => s,
            Err(err) => {
                return Json(ProjectEnvironmentConfigResetResponse {
                    status: "error".into(),
                    error: Some(format!("parse stack: {err}")),
                    config: None,
                });
            }
        };

        let cfg = djinn_stack::environment::EnvironmentConfig::from_stack(&stack);
        if let Err(err) = cfg.validate() {
            return Json(ProjectEnvironmentConfigResetResponse {
                status: "error".into(),
                error: Some(format!("validate: {err}")),
                config: None,
            });
        }

        if let Err(err) = self
            .state
            .apply_environment_config(&input.project, &cfg)
            .await
        {
            return Json(ProjectEnvironmentConfigResetResponse {
                status: "error".into(),
                error: Some(format!("apply: {err}")),
                config: None,
            });
        }

        let json = match serde_json::to_value(&cfg) {
            Ok(serde_json::Value::Object(map)) => {
                Some(ObjectJson::from(serde_json::Value::Object(map)))
            }
            _ => None,
        };
        Json(ProjectEnvironmentConfigResetResponse {
            status: "ok".into(),
            error: None,
            config: json,
        })
    }
}
