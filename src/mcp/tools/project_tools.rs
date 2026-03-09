use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::commands::{CommandSpec, run_commands};
use crate::db::repositories::git_settings::GitSettingsRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::mcp::server::DjinnMcpServer;

const DJINN_GITIGNORE: &str = "worktrees/\n";

/// Ensure the project directory is a git repo with at least one commit.
///
/// Handles:
/// 1. Not a git repo → `git init`.
/// 2. No commits on HEAD → stage `.djinn/.gitignore` and create initial commit.
/// 3. Already has commits → no-op.
async fn ensure_git_repo_ready(path: &str) -> Result<(), String> {
    let project_path = std::path::PathBuf::from(path);
    let git_dir = project_path.join(".git");

    // 1. Initialize git repo if needed.
    if !git_dir.exists() {
        tracing::info!(path, "project_add: initializing git repo");
        let output = tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&project_path)
            .output()
            .await
            .map_err(|e| format!("git init failed: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    // 2. Check if HEAD points to a valid commit.
    let rev_parse = tokio::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(&project_path)
        .output()
        .await
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    if rev_parse.status.success() {
        return Ok(()); // Already has commits.
    }

    // 3. Stage .djinn/.gitignore and create initial commit.
    tracing::info!(path, "project_add: creating initial commit");
    let add = tokio::process::Command::new("git")
        .args(["add", ".djinn/.gitignore"])
        .current_dir(&project_path)
        .output()
        .await
        .map_err(|e| format!("git add failed: {e}"))?;
    if !add.status.success() {
        return Err(format!(
            "git add .djinn/.gitignore failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }

    let commit = tokio::process::Command::new("git")
        .args([
            "commit",
            "--no-verify",
            "-m",
            "chore: initialize repository",
        ])
        .current_dir(&project_path)
        .output()
        .await
        .map_err(|e| format!("git commit failed: {e}"))?;
    if !commit.status.success() {
        return Err(format!(
            "initial commit failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        ));
    }

    Ok(())
}

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

#[derive(Serialize, JsonSchema)]
pub struct ProjectConfigResponse {
    pub status: String,
    pub project: String,
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
}

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

// ── Command structs ──────────────────────────────────────────────────────────

/// A single command entry in a project's setup or verification list.
#[derive(Deserialize, Serialize, JsonSchema, Clone)]
pub struct ProjectCommandSpec {
    /// Human-readable label for this command.
    pub name: String,
    /// Shell command executed via `sh -c`.
    pub command: String,
    /// Optional timeout in seconds (default: 300).
    pub timeout_secs: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectCommandsGetParams {
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectCommandsSetParams {
    /// Absolute project path.
    pub project: String,
    /// Full replacement for setup commands. Omit to keep existing.
    pub setup_commands: Option<Vec<ProjectCommandSpec>>,
    /// Full replacement for verification commands. Omit to keep existing.
    pub verification_commands: Option<Vec<ProjectCommandSpec>>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectCommandsGetResponse {
    pub status: String,
    pub project: String,
    pub setup_commands: Vec<ProjectCommandSpec>,
    pub verification_commands: Vec<ProjectCommandSpec>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectCommandsSetResponse {
    pub status: String,
    pub project: String,
    /// Commands that failed validation (non-zero exit). Empty on success.
    pub validation_errors: Vec<CommandValidationError>,
}

#[derive(Serialize, JsonSchema)]
pub struct CommandValidationError {
    pub command_name: String,
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

fn parse_command_specs(json: &str) -> Vec<ProjectCommandSpec> {
    serde_json::from_str(json).unwrap_or_default()
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

        // Ensure .djinn/ directory and .gitignore exist
        let djinn_dir = Path::new(path).join(".djinn");
        let _ = fs::create_dir_all(&djinn_dir).await;
        let gitignore_path = djinn_dir.join(".gitignore");
        if !gitignore_path.exists() {
            let _ = fs::write(&gitignore_path, DJINN_GITIGNORE).await;
        }

        // Ensure the project is a git repo with at least one commit.
        if let Err(e) = ensure_git_repo_ready(path).await {
            tracing::warn!(path, error = %e, "project_add: git bootstrap failed");
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
            Err(_) => Json(ProjectListResponse { projects: vec![] }),
        }
    }

    #[tool(description = "Get project config fields for a project path.")]
    pub async fn project_config_get(
        &self,
        Parameters(input): Parameters<ProjectConfigGetParams>,
    ) -> Json<ProjectConfigResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.get_by_path(&input.project).await {
            Ok(Some(project)) => Json(ProjectConfigResponse {
                status: "ok".into(),
                project: project.path,
                target_branch: project.target_branch,
                auto_merge: project.auto_merge,
                sync_enabled: project.sync_enabled,
                sync_remote: project.sync_remote,
            }),
            Ok(None) => Json(ProjectConfigResponse {
                status: format!("error: project not found: {}", input.project),
                project: input.project,
                target_branch: "main".into(),
                auto_merge: true,
                sync_enabled: false,
                sync_remote: None,
            }),
            Err(e) => Json(ProjectConfigResponse {
                status: format!("error: {e}"),
                project: input.project,
                target_branch: "main".into(),
                auto_merge: true,
                sync_enabled: false,
                sync_remote: None,
            }),
        }
    }

    #[tool(description = "Set a single project config field by key.")]
    pub async fn project_config_set(
        &self,
        Parameters(input): Parameters<ProjectConfigSetParams>,
    ) -> Json<ProjectConfigResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());
        let project = match repo.get_by_path(&input.project).await {
            Ok(Some(project)) => project,
            Ok(None) => {
                return Json(ProjectConfigResponse {
                    status: format!("error: project not found: {}", input.project),
                    project: input.project,
                    target_branch: "main".into(),
                    auto_merge: true,
                    sync_enabled: false,
                    sync_remote: None,
                });
            }
            Err(e) => {
                return Json(ProjectConfigResponse {
                    status: format!("error: {e}"),
                    project: input.project,
                    target_branch: "main".into(),
                    auto_merge: true,
                    sync_enabled: false,
                    sync_remote: None,
                });
            }
        };

        match repo
            .update_config_field(&project.id, &input.key, &input.value)
            .await
        {
            Ok(Some(config)) => Json(ProjectConfigResponse {
                status: "ok".into(),
                project: project.path,
                target_branch: config.target_branch,
                auto_merge: config.auto_merge,
                sync_enabled: config.sync_enabled,
                sync_remote: config.sync_remote,
            }),
            Ok(None) => Json(ProjectConfigResponse {
                status: format!("error: invalid key '{}'", input.key),
                project: project.path,
                target_branch: project.target_branch,
                auto_merge: project.auto_merge,
                sync_enabled: project.sync_enabled,
                sync_remote: project.sync_remote,
            }),
            Err(e) => Json(ProjectConfigResponse {
                status: format!("error: {e}"),
                project: project.path,
                target_branch: project.target_branch,
                auto_merge: project.auto_merge,
                sync_enabled: project.sync_enabled,
                sync_remote: project.sync_remote,
            }),
        }
    }
    /// Return the configured setup and verification commands for a project.
    #[tool(description = "Read setup and verification commands configured for a project.")]
    pub async fn project_commands_get(
        &self,
        Parameters(input): Parameters<ProjectCommandsGetParams>,
    ) -> Json<ProjectCommandsGetResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.get_by_path(&input.project).await {
            Ok(Some(project)) => Json(ProjectCommandsGetResponse {
                status: "ok".to_string(),
                project: project.path,
                setup_commands: parse_command_specs(&project.setup_commands),
                verification_commands: parse_command_specs(&project.verification_commands),
            }),
            Ok(None) => Json(ProjectCommandsGetResponse {
                status: format!("error: project not found: {}", input.project),
                project: input.project,
                setup_commands: vec![],
                verification_commands: vec![],
            }),
            Err(e) => Json(ProjectCommandsGetResponse {
                status: format!("error: {e}"),
                project: input.project,
                setup_commands: vec![],
                verification_commands: vec![],
            }),
        }
    }

    /// Set setup and/or verification commands for a project.
    ///
    /// Commands are validated by running them in a temporary worktree before
    /// saving. If validation fails, the configuration is NOT saved and the
    /// failing command's output is returned.
    #[tool(
        description = "Set setup and/or verification commands for a project. Commands are validated in a temporary worktree before saving. If validation fails, nothing is saved and the failing output is returned."
    )]
    pub async fn project_commands_set(
        &self,
        Parameters(input): Parameters<ProjectCommandsSetParams>,
    ) -> Json<ProjectCommandsSetResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.events().clone());

        let project = match repo.get_by_path(&input.project).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Json(ProjectCommandsSetResponse {
                    status: format!("error: project not found: {}", input.project),
                    project: input.project,
                    validation_errors: vec![],
                });
            }
            Err(e) => {
                return Json(ProjectCommandsSetResponse {
                    status: format!("error: {e}"),
                    project: input.project,
                    validation_errors: vec![],
                });
            }
        };

        // Reject if project has active dispatch (commands could run mid-update).
        if let Some(coordinator) = self.state.coordinator().await
            && let Ok(status) = coordinator.get_project_status(&project.id)
            && !status.paused
        {
            return Json(ProjectCommandsSetResponse {
                status: "error: project has active dispatch — pause execution first (execution_pause) before updating commands".to_string(),
                project: project.path,
                validation_errors: vec![],
            });
        }

        // Merge new with existing (keep existing when not provided).
        let new_setup = input
            .setup_commands
            .unwrap_or_else(|| parse_command_specs(&project.setup_commands));
        let new_verification = input
            .verification_commands
            .unwrap_or_else(|| parse_command_specs(&project.verification_commands));

        // If no commands to validate, skip worktree creation.
        let mut validation_errors: Vec<CommandValidationError> = vec![];

        if !new_setup.is_empty() || !new_verification.is_empty() {
            let project_path = Path::new(&project.path);

            let git_actor = match self.state.git_actor(project_path).await {
                Ok(a) => a,
                Err(e) => {
                    return Json(ProjectCommandsSetResponse {
                        status: format!("error: failed to get git actor: {e}"),
                        project: project.path,
                        validation_errors: vec![],
                    });
                }
            };

            let git_settings =
                GitSettingsRepository::new(self.state.db().clone(), self.state.events().clone())
                    .get(&project.id)
                    .await
                    .unwrap_or_default();

            let wt_name = format!("cmd-validate-{}", uuid::Uuid::now_v7());
            let wt_path = match git_actor
                .create_worktree(&wt_name, &git_settings.target_branch, true)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    return Json(ProjectCommandsSetResponse {
                        status: format!("error: failed to create validation worktree: {e}"),
                        project: project.path,
                        validation_errors: vec![],
                    });
                }
            };

            // Run setup commands.
            let setup_specs: Vec<CommandSpec> = new_setup
                .iter()
                .map(|c| CommandSpec {
                    name: c.name.clone(),
                    command: c.command.clone(),
                    timeout_secs: c.timeout_secs.map(|t| t as u64),
                })
                .collect();

            let mut setup_failed = false;
            if let Ok(results) = run_commands(&setup_specs, &wt_path).await {
                for r in results {
                    if r.exit_code != 0 {
                        validation_errors.push(CommandValidationError {
                            command_name: r.name,
                            exit_code: r.exit_code as i64,
                            stdout: r.stdout,
                            stderr: r.stderr,
                        });
                        setup_failed = true;
                    }
                }
            }

            // Run verification commands only if setup passed.
            if !setup_failed {
                let verification_specs: Vec<CommandSpec> = new_verification
                    .iter()
                    .map(|c| CommandSpec {
                        name: c.name.clone(),
                        command: c.command.clone(),
                        timeout_secs: c.timeout_secs.map(|t| t as u64),
                    })
                    .collect();

                if let Ok(results) = run_commands(&verification_specs, &wt_path).await {
                    for r in results {
                        if r.exit_code != 0 {
                            validation_errors.push(CommandValidationError {
                                command_name: r.name,
                                exit_code: r.exit_code as i64,
                                stdout: r.stdout,
                                stderr: r.stderr,
                            });
                        }
                    }
                }
            }

            // Always clean up the worktree.
            let _ = git_actor.remove_worktree(&wt_path).await;
        }

        if !validation_errors.is_empty() {
            return Json(ProjectCommandsSetResponse {
                status: "validation_failed".to_string(),
                project: project.path,
                validation_errors,
            });
        }

        // Persist.
        let setup_json = serde_json::to_string(&new_setup).unwrap_or_else(|_| "[]".to_string());
        let verification_json =
            serde_json::to_string(&new_verification).unwrap_or_else(|_| "[]".to_string());

        match repo
            .update_commands(&project.id, &setup_json, &verification_json)
            .await
        {
            Ok(_) => Json(ProjectCommandsSetResponse {
                status: "ok".to_string(),
                project: project.path,
                validation_errors: vec![],
            }),
            Err(e) => Json(ProjectCommandsSetResponse {
                status: format!("error: {e}"),
                project: project.path,
                validation_errors: vec![],
            }),
        }
    }
}
