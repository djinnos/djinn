use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::db::repositories::project::ProjectRepository;
use crate::mcp::server::DjinnMcpServer;

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProjectAddParams {
    /// Human-readable project name (unique identifier).
    pub name: String,
    /// Absolute path to the project directory.
    pub path: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectRemoveParams {
    /// Project name to remove.
    pub name: String,
}

// ── Response structs ─────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProjectAddResponse {
    pub status: String,
    pub project: ProjectInfo,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectRemoveResponse {
    pub status: String,
    pub project: ProjectInfo,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectListResponse {
    pub projects: Vec<ProjectInfo>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub path: String,
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = project_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Register a project directory with Djinn.
    #[tool(
        description = "Add a project to the Djinn registry. Validates that the path exists. Idempotent: re-adding the same name+path is a no-op."
    )]
    pub async fn project_add(
        &self,
        Parameters(input): Parameters<ProjectAddParams>,
    ) -> Json<ProjectAddResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());
        let path = input.path.trim_end_matches('/');

        // Validate path exists
        if !Path::new(path).is_dir() {
            return Json(ProjectAddResponse {
                status: format!("error: path does not exist or is not a directory: {path}"),
                project: ProjectInfo {
                    id: String::new(),
                    name: input.name,
                    path: path.to_string(),
                },
            });
        }

        // Idempotent: if same name+path already exists, return it
        if let Ok(Some(existing)) = repo.get_by_path(path).await {
            if existing.name == input.name {
                return Json(ProjectAddResponse {
                    status: "ok".to_string(),
                    project: ProjectInfo {
                        id: existing.id,
                        name: existing.name,
                        path: existing.path,
                    },
                });
            }
            // Path exists but under a different name
            return Json(ProjectAddResponse {
                status: format!(
                    "error: path already registered under name '{}'",
                    existing.name
                ),
                project: ProjectInfo {
                    id: String::new(),
                    name: input.name,
                    path: path.to_string(),
                },
            });
        }

        match repo.create(&input.name, path).await {
            Ok(project) => Json(ProjectAddResponse {
                status: "ok".to_string(),
                project: ProjectInfo {
                    id: project.id,
                    name: project.name,
                    path: project.path,
                },
            }),
            Err(e) => Json(ProjectAddResponse {
                status: format!("error: {e}"),
                project: ProjectInfo {
                    id: String::new(),
                    name: input.name,
                    path: path.to_string(),
                },
            }),
        }
    }

    /// Unregister a project from Djinn.
    #[tool(description = "Remove a project from the Djinn registry by name.")]
    pub async fn project_remove(
        &self,
        Parameters(input): Parameters<ProjectRemoveParams>,
    ) -> Json<ProjectRemoveResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());

        // Find the project by name
        let projects = match repo.list().await {
            Ok(p) => p,
            Err(e) => {
                return Json(ProjectRemoveResponse {
                    status: format!("error: {e}"),
                    project: ProjectInfo {
                        id: String::new(),
                        name: input.name,
                        path: String::new(),
                    },
                });
            }
        };

        let Some(project) = projects.into_iter().find(|p| p.name == input.name) else {
            return Json(ProjectRemoveResponse {
                status: format!("error: project '{}' not found", input.name),
                project: ProjectInfo {
                    id: String::new(),
                    name: input.name,
                    path: String::new(),
                },
            });
        };

        let info = ProjectInfo {
            id: project.id.clone(),
            name: project.name.clone(),
            path: project.path.clone(),
        };

        match repo.delete(&project.id).await {
            Ok(()) => Json(ProjectRemoveResponse {
                status: "ok".to_string(),
                project: info,
            }),
            Err(e) => Json(ProjectRemoveResponse {
                status: format!("error: {e}"),
                project: info,
            }),
        }
    }

    /// List all registered projects.
    #[tool(description = "List all projects registered with Djinn.")]
    pub async fn project_list(&self) -> Json<ProjectListResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());

        match repo.list().await {
            Ok(projects) => Json(ProjectListResponse {
                projects: projects
                    .into_iter()
                    .map(|p| ProjectInfo {
                        id: p.id,
                        name: p.name,
                        path: p.path,
                    })
                    .collect(),
            }),
            Err(_) => Json(ProjectListResponse {
                projects: vec![],
            }),
        }
    }
}
