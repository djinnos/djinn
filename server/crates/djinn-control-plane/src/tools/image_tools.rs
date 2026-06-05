//! Registered image catalog MCP tools (Phase B).
//!
//! A registered image is a NAMED `EnvironmentConfig` (build fields). Picking an
//! image for a project applies that config via the existing
//! `apply_environment_config` path — which builds the image + triggers the
//! rebuild — so the catalog reuses the live build/dispatch pipeline unchanged.
//! Editing an image re-applies it to every project assigned to it.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::ObjectJson;
use djinn_db::{ImageRepository, ProjectRepository};

#[derive(Deserialize, JsonSchema)]
pub struct ImageListParams {}

#[derive(Serialize, JsonSchema)]
pub struct ImageDto {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Build status of the image's own config: none | building | ready | failed.
    pub status: String,
    /// The image's EnvironmentConfig (build fields).
    pub config: ObjectJson,
}

#[derive(Serialize, JsonSchema)]
pub struct ImageListResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub images: Vec<ImageDto>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImageCreateParams {
    /// Unique display name (e.g. "Go", "Rust", "Node").
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The image's EnvironmentConfig (languages+versions, system_packages,
    /// build env, post_build hooks). Validated server-side.
    pub config: ObjectJson,
}

#[derive(Serialize, JsonSchema)]
pub struct ImageMutateResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImageUpdateParams {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub config: ObjectJson,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImageDeleteParams {
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectSetImageParams {
    /// Project UUID or `owner/repo` slug.
    pub project: String,
    /// Image id to assign, or null/omit to clear the assignment (the project
    /// keeps its current environment config).
    #[serde(default)]
    pub image_id: Option<String>,
}

fn parse_validated_config(
    config: ObjectJson,
) -> Result<djinn_stack::environment::EnvironmentConfig, String> {
    let cfg: djinn_stack::environment::EnvironmentConfig =
        serde_json::from_value(serde_json::Value::Object(config.0))
            .map_err(|e| format!("parse config: {e}"))?;
    cfg.validate().map_err(|e| format!("validate: {e}"))?;
    Ok(cfg)
}

#[tool_router(router = image_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(description = "List registered catalog images (name, status, config).")]
    pub async fn image_list(
        &self,
        Parameters(_): Parameters<ImageListParams>,
    ) -> Json<ImageListResponse> {
        let repo = ImageRepository::new(self.state.db().clone());
        match repo.list().await {
            Ok(rows) => {
                let images = rows
                    .into_iter()
                    .map(|i| ImageDto {
                        id: i.id,
                        name: i.name,
                        description: i.description,
                        status: i.status,
                        config: ObjectJson::from(
                            serde_json::from_str::<serde_json::Value>(&i.config)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        ),
                    })
                    .collect();
                Json(ImageListResponse {
                    status: "ok".into(),
                    error: None,
                    images,
                })
            }
            Err(e) => Json(ImageListResponse {
                status: "error".into(),
                error: Some(format!("db error: {e}")),
                images: Vec::new(),
            }),
        }
    }

    #[tool(
        description = "Register a new catalog image from a name + EnvironmentConfig. Projects pick it via project_set_image."
    )]
    pub async fn image_create(
        &self,
        Parameters(input): Parameters<ImageCreateParams>,
    ) -> Json<ImageMutateResponse> {
        let cfg = match parse_validated_config(input.config) {
            Ok(c) => c,
            Err(e) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(e),
                    id: None,
                });
            }
        };
        let config_json = match serde_json::to_string(&cfg) {
            Ok(j) => j,
            Err(e) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(format!("serialize config: {e}")),
                    id: None,
                });
            }
        };
        let id = uuid::Uuid::now_v7().to_string();
        let repo = ImageRepository::new(self.state.db().clone());
        match repo
            .create(&id, &input.name, input.description.as_deref(), &config_json)
            .await
        {
            Ok(()) => Json(ImageMutateResponse {
                status: "ok".into(),
                error: None,
                id: Some(id),
            }),
            Err(e) => Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("create: {e}")),
                id: None,
            }),
        }
    }

    #[tool(
        description = "Update a catalog image's name/description/config. Re-applies the new config to every project assigned to this image (triggering their rebuild)."
    )]
    pub async fn image_update(
        &self,
        Parameters(input): Parameters<ImageUpdateParams>,
    ) -> Json<ImageMutateResponse> {
        let cfg = match parse_validated_config(input.config) {
            Ok(c) => c,
            Err(e) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(e),
                    id: None,
                });
            }
        };
        let config_json = match serde_json::to_string(&cfg) {
            Ok(j) => j,
            Err(e) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(format!("serialize config: {e}")),
                    id: None,
                });
            }
        };
        let repo = ImageRepository::new(self.state.db().clone());
        if let Err(e) = repo
            .update(&input.id, &input.name, input.description.as_deref(), &config_json)
            .await
        {
            return Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("update: {e}")),
                id: None,
            });
        }
        // Fan out the new config to assigned projects so they rebuild.
        match repo.projects_using(&input.id).await {
            Ok(projects) => {
                for project_id in projects {
                    if let Err(e) = self.state.apply_environment_config(&project_id, &cfg).await {
                        tracing::warn!(
                            project_id = %project_id,
                            image_id = %input.id,
                            error = %e,
                            "image_update: re-apply to assigned project failed"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(image_id = %input.id, error = %e, "image_update: list assigned projects failed");
            }
        }
        Json(ImageMutateResponse {
            status: "ok".into(),
            error: None,
            id: Some(input.id),
        })
    }

    #[tool(
        description = "Delete a catalog image. Fails if a project still references it (reassign those projects first)."
    )]
    pub async fn image_delete(
        &self,
        Parameters(input): Parameters<ImageDeleteParams>,
    ) -> Json<ImageMutateResponse> {
        let repo = ImageRepository::new(self.state.db().clone());
        match repo.delete(&input.id).await {
            Ok(()) => Json(ImageMutateResponse {
                status: "ok".into(),
                error: None,
                id: Some(input.id),
            }),
            Err(e) => Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!(
                    "delete failed (is a project still using it?): {e}"
                )),
                id: None,
            }),
        }
    }

    #[tool(
        description = "Assign a catalog image to a project (applies its config + triggers a rebuild), or pass image_id=null to clear the assignment."
    )]
    pub async fn project_set_image(
        &self,
        Parameters(input): Parameters<ProjectSetImageParams>,
    ) -> Json<ImageMutateResponse> {
        let project_repo =
            ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match project_repo.resolve(&input.project).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                    id: None,
                });
            }
            Err(e) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {e}")),
                    id: None,
                });
            }
        };
        let image_repo = ImageRepository::new(self.state.db().clone());

        let Some(image_id) = input.image_id.as_deref().filter(|s| !s.is_empty()) else {
            // Clear the assignment; leave the project's current env config as-is.
            return match image_repo.set_project_image(&project_id, None).await {
                Ok(()) => Json(ImageMutateResponse {
                    status: "ok".into(),
                    error: None,
                    id: None,
                }),
                Err(e) => Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(format!("clear assignment: {e}")),
                    id: None,
                }),
            };
        };

        let image = match image_repo.get(image_id).await {
            Ok(Some(i)) => i,
            Ok(None) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(format!("image not found: {image_id}")),
                    id: None,
                });
            }
            Err(e) => {
                return Json(ImageMutateResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {e}")),
                    id: None,
                });
            }
        };
        let mut cfg: djinn_stack::environment::EnvironmentConfig =
            match serde_json::from_str(&image.config) {
                Ok(c) => c,
                Err(e) => {
                    return Json(ImageMutateResponse {
                        status: "error".into(),
                        error: Some(format!("image config parse: {e}")),
                        id: None,
                    });
                }
            };
        // Mark user-edited so the boot reseed never clobbers an applied image.
        cfg.source = djinn_stack::environment::ConfigSource::UserEdited;

        if let Err(e) = image_repo.set_project_image(&project_id, Some(image_id)).await {
            return Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("assign: {e}")),
                id: None,
            });
        }
        // Apply the image's config — reuses the live build/dispatch pipeline
        // (writes environment_config + nulls image_hash → next tick rebuilds).
        if let Err(e) = self.state.apply_environment_config(&project_id, &cfg).await {
            return Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("apply image config: {e}")),
                id: None,
            });
        }
        Json(ImageMutateResponse {
            status: "ok".into(),
            error: None,
            id: Some(image_id.to_string()),
        })
    }
}
