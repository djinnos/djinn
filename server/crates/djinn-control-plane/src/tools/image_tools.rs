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
    #[schemars(with = "djinn_stack::environment::EnvironmentConfig")]
    pub config: ObjectJson,
    /// Service-preset ids injected as native sidecars into every Pod that runs
    /// this image (e.g. a Postgres reachable on 127.0.0.1 with TEST_POSTGRES_URL).
    #[serde(default)]
    pub service_presets: Vec<String>,
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
    #[schemars(with = "djinn_stack::environment::EnvironmentConfig")]
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
    /// The image's EnvironmentConfig (build fields). Validated server-side.
    #[schemars(with = "djinn_stack::environment::EnvironmentConfig")]
    pub config: ObjectJson,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImageDeleteParams {
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImageSetServicesParams {
    /// Image id.
    pub id: String,
    /// Service-preset ids to inject as native sidecars into every Pod that runs
    /// this image (full replacement). Empty clears all injected services.
    pub preset_ids: Vec<String>,
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

#[derive(Deserialize, JsonSchema)]
pub struct ToolchainVersionsParams {}

#[derive(Serialize, JsonSchema)]
pub struct ToolchainVersionsResponse {
    pub status: String,
    /// Available versions per language (rust/node/python/go/java/ruby/dotnet/clang),
    /// fetched live from upstream (cached) with static fallback.
    pub versions: std::collections::BTreeMap<String, Vec<String>>,
}

#[tool_router(router = image_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "List available toolchain versions per language for the image version selectors (live from upstream, cached)."
    )]
    pub async fn toolchain_versions(
        &self,
        Parameters(_): Parameters<ToolchainVersionsParams>,
    ) -> Json<ToolchainVersionsResponse> {
        Json(ToolchainVersionsResponse {
            status: "ok".into(),
            versions: crate::toolchain_versions::fetch_toolchain_versions().await,
        })
    }

    #[tool(description = "List registered catalog images (name, status, config).")]
    pub async fn image_list(
        &self,
        Parameters(_): Parameters<ImageListParams>,
    ) -> Json<ImageListResponse> {
        let repo = ImageRepository::new(self.state.db().clone());
        match repo.list().await {
            Ok(rows) => {
                let mut images = Vec::with_capacity(rows.len());
                for i in rows {
                    let service_presets =
                        repo.list_service_presets(&i.id).await.unwrap_or_default();
                    images.push(ImageDto {
                        id: i.id,
                        name: i.name,
                        description: i.description,
                        status: i.status,
                        config: ObjectJson::from(
                            serde_json::from_str::<serde_json::Value>(&i.config)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        ),
                        service_presets,
                    });
                }
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
            Ok(()) => {
                // Build the shared image immediately so the catalog badge
                // reaches `ready` without waiting for a project to be assigned.
                if let Err(e) = self.state.enqueue_image_build(&id).await {
                    tracing::warn!(
                        image_id = %id,
                        error = %e,
                        "image_create: enqueue build failed; next reconcile tick will retry"
                    );
                }
                Json(ImageMutateResponse {
                    status: "ok".into(),
                    error: None,
                    id: Some(id),
                })
            }
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
            .update(
                &input.id,
                &input.name,
                input.description.as_deref(),
                &config_json,
            )
            .await
        {
            return Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("update: {e}")),
                id: None,
            });
        }
        // `update` reset the image's build state (status→none, hash/tag
        // cleared). Rebuild the single shared image once...
        if let Err(e) = self.state.enqueue_image_build(&input.id).await {
            tracing::warn!(
                image_id = %input.id,
                error = %e,
                "image_update: enqueue shared image rebuild failed; next tick will retry"
            );
        }
        // ...then re-warm every project on it, since the runtime image their
        // canonical graph indexes against has changed. (No config fan-out —
        // the config lives on the image, not copied into each project.)
        match repo.projects_using(&input.id).await {
            Ok(projects) => {
                for project_id in projects {
                    self.state.trigger_graph_warm(&project_id).await;
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
                error: Some(format!("delete failed (is a project still using it?): {e}")),
                id: None,
            }),
        }
    }

    #[tool(
        description = "Set which backing-service presets are injected as native sidecars into every task-run Pod that runs this image (full replacement; empty clears all). Each becomes reachable on 127.0.0.1 with its connection string in the preset's env var (e.g. TEST_POSTGRES_URL)."
    )]
    pub async fn image_set_services(
        &self,
        Parameters(input): Parameters<ImageSetServicesParams>,
    ) -> Json<ImageMutateResponse> {
        let repo = ImageRepository::new(self.state.db().clone());
        match repo.set_service_presets(&input.id, &input.preset_ids).await {
            Ok(()) => Json(ImageMutateResponse {
                status: "ok".into(),
                error: None,
                id: Some(input.id),
            }),
            Err(e) => Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("set image services: {e}")),
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
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
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
        // The image must hold a parseable EnvironmentConfig — validate the
        // selection up front, but do NOT copy it into the project. A
        // catalogued project shares the catalog image; it does not build (or
        // dispatch against) its own per-project image (migration 46).
        if let Err(e) =
            serde_json::from_str::<djinn_stack::environment::EnvironmentConfig>(&image.config)
        {
            return Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("image config parse: {e}")),
                id: None,
            });
        }

        if let Err(e) = image_repo
            .set_project_image(&project_id, Some(image_id))
            .await
        {
            return Json(ImageMutateResponse {
                status: "error".into(),
                error: Some(format!("assign: {e}")),
                id: None,
            });
        }

        // Ensure the shared image is built (idempotent — no-op if already
        // ready) so this project can dispatch against it. The assignment is
        // already durable at this point, so an enqueue failure is not an
        // assignment failure: the periodic catalog reconciler will retry it.
        // Returning an error here used to make clients retry a mutation that
        // had in fact succeeded.
        if let Err(e) = self.state.enqueue_image_build(image_id).await {
            tracing::warn!(
                image_id,
                project_id,
                error = %e,
                "project_set_image: assignment persisted but build enqueue failed; reconcile will retry"
            );
        }
        // Warm this project's canonical graph against the (shared) image. If
        // the image isn't ready yet this no-ops; the build watcher re-fires
        // the warm for every assigned project once the image goes ready.
        self.state.trigger_graph_warm(&project_id).await;

        Json(ImageMutateResponse {
            status: "ok".into(),
            error: None,
            id: Some(image_id.to_string()),
        })
    }
}
