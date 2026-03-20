use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::server::DjinnMcpServer;
use djinn_db::{ProjectRepository, VerificationRule};

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
        let mut cmd = std::process::Command::new("git");
        cmd.args(["init"]).current_dir(&project_path);
        let output = crate::process::output(cmd)
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
    let mut cmd = std::process::Command::new("git");
    cmd.args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(&project_path);
    let rev_parse = crate::process::output(cmd)
        .await
        .map_err(|e| format!("git rev-parse failed: {e}"))?;

    if rev_parse.status.success() {
        return Ok(()); // Already has commits.
    }

    // 3. Stage .djinn/.gitignore and create initial commit.
    tracing::info!(path, "project_add: creating initial commit");
    let mut cmd = std::process::Command::new("git");
    cmd.args(["add", ".djinn/.gitignore"])
        .current_dir(&project_path);
    let add = crate::process::output(cmd)
        .await
        .map_err(|e| format!("git add failed: {e}"))?;
    if !add.status.success() {
        return Err(format!(
            "git add .djinn/.gitignore failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }

    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "commit",
        "--no-verify",
        "-m",
        "chore: initialize repository",
    ])
    .current_dir(&project_path);
    let commit = crate::process::output(cmd)
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
    /// Absolute path of the project to remove. Must match the registered path exactly.
    pub path: String,
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

/// A single verification rule returned in project config.
#[derive(Deserialize, Serialize, JsonSchema, Clone)]
pub struct VerificationRuleDto {
    /// Glob pattern (e.g. `src/**/*.rs`, `**` for catch-all).
    pub match_pattern: String,
    /// One or more shell commands to run when the pattern matches.
    pub commands: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectConfigResponse {
    pub status: String,
    pub project: String,
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
    /// File-pattern-to-command mapping for selective verification.
    /// Empty list means fall back to full-project verification.
    pub verification_rules: Vec<VerificationRuleDto>,
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
pub struct ProjectSettingsValidateParams {
    /// Absolute path to the worktree containing .djinn/settings.json
    pub worktree_path: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectSettingsValidateResponse {
    pub valid: bool,
    pub errors: Vec<String>,
}

#[derive(Deserialize)]
struct StrictDjinnSettings {
    #[serde(default, rename = "setup")]
    _setup: Vec<ProjectCommandSpec>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parse a JSON-encoded `verification_rules` string into `Vec<VerificationRuleDto>`.
/// Returns an empty vec on any parse error (safe default).
fn parse_verification_rules(json: &str) -> Vec<VerificationRuleDto> {
    serde_json::from_str::<Vec<VerificationRule>>(json)
        .unwrap_or_default()
        .into_iter()
        .map(|r| VerificationRuleDto {
            match_pattern: r.match_pattern,
            commands: r.commands,
        })
        .collect()
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
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
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
    #[tool(
        description = "Remove a project from the Djinn registry by name and path. Both name and path must match exactly to prevent accidental deletion when duplicate names exist."
    )]
    pub async fn project_remove(
        &self,
        Parameters(input): Parameters<ProjectRemoveParams>,
    ) -> Json<ProjectRemoveResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // Find the project by name AND path to prevent accidental deletion of duplicates
        let projects = match repo.list().await {
            Ok(p) => p,
            Err(e) => {
                return Json(ProjectRemoveResponse {
                    status: format!("error: {e}"),
                    project: ProjectInfo {
                        id: String::new(),
                        name: input.name,
                        path: input.path,
                    },
                });
            }
        };

        let path = input.path.trim_end_matches('/');
        let Some(project) = projects
            .into_iter()
            .find(|p| p.name == input.name && p.path.trim_end_matches('/') == path)
        else {
            return Json(ProjectRemoveResponse {
                status: format!(
                    "error: no project named '{}' with path '{}' found",
                    input.name, path
                ),
                project: ProjectInfo {
                    id: String::new(),
                    name: input.name,
                    path: path.to_string(),
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
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

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
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match repo.get_by_path(&input.project).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Json(ProjectConfigResponse {
                    status: format!("error: project not found: {}", input.project),
                    project: input.project,
                    target_branch: "main".into(),
                    auto_merge: true,
                    sync_enabled: false,
                    sync_remote: None,
                    verification_rules: vec![],
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
                    verification_rules: vec![],
                });
            }
        };
        match repo.get_config(&project.id).await {
            Ok(Some(config)) => Json(ProjectConfigResponse {
                status: "ok".into(),
                project: project.path,
                target_branch: config.target_branch,
                auto_merge: config.auto_merge,
                sync_enabled: config.sync_enabled,
                sync_remote: config.sync_remote,
                verification_rules: parse_verification_rules(&config.verification_rules),
            }),
            Ok(None) => Json(ProjectConfigResponse {
                status: "ok".into(),
                project: project.path,
                target_branch: project.target_branch,
                auto_merge: project.auto_merge,
                sync_enabled: project.sync_enabled,
                sync_remote: project.sync_remote,
                verification_rules: vec![],
            }),
            Err(e) => Json(ProjectConfigResponse {
                status: format!("error: {e}"),
                project: project.path,
                target_branch: project.target_branch,
                auto_merge: project.auto_merge,
                sync_enabled: project.sync_enabled,
                sync_remote: project.sync_remote,
                verification_rules: vec![],
            }),
        }
    }

    #[tool(description = "Set a single project config field by key.")]
    pub async fn project_config_set(
        &self,
        Parameters(input): Parameters<ProjectConfigSetParams>,
    ) -> Json<ProjectConfigResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
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
                    verification_rules: vec![],
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
                    verification_rules: vec![],
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
                verification_rules: parse_verification_rules(&config.verification_rules),
            }),
            Ok(None) => Json(ProjectConfigResponse {
                status: format!("error: invalid key '{}'", input.key),
                project: project.path,
                target_branch: project.target_branch,
                auto_merge: project.auto_merge,
                sync_enabled: project.sync_enabled,
                sync_remote: project.sync_remote,
                verification_rules: vec![],
            }),
            Err(e) => Json(ProjectConfigResponse {
                status: format!("error: {e}"),
                project: project.path,
                target_branch: project.target_branch,
                auto_merge: project.auto_merge,
                sync_enabled: project.sync_enabled,
                sync_remote: project.sync_remote,
                verification_rules: vec![],
            }),
        }
    }
    #[tool(description = "Validate .djinn/settings.json syntax and schema in a worktree.")]
    pub async fn project_settings_validate(
        &self,
        Parameters(input): Parameters<ProjectSettingsValidateParams>,
    ) -> Json<ProjectSettingsValidateResponse> {
        let settings_path = Path::new(&input.worktree_path).join(".djinn/settings.json");
        let mut errors = Vec::new();

        let content = match std::fs::read_to_string(&settings_path) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("failed to read {}: {e}", settings_path.display()));
                return Json(ProjectSettingsValidateResponse {
                    valid: false,
                    errors,
                });
            }
        };

        let value: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("invalid JSON syntax: {e}"));
                return Json(ProjectSettingsValidateResponse {
                    valid: false,
                    errors,
                });
            }
        };

        if let serde_json::Value::Object(map) = &value {
            for key in map.keys() {
                if key != "setup" && key != "verification" {
                    errors.push(format!(
                        "warning: unknown top-level key '{key}' (allowed: setup, verification)"
                    ));
                }
            }
        }

        if let Err(e) = serde_json::from_value::<StrictDjinnSettings>(value) {
            errors.push(format!("schema validation failed: {e}"));
            return Json(ProjectSettingsValidateResponse {
                valid: false,
                errors,
            });
        }

        Json(ProjectSettingsValidateResponse {
            valid: true,
            errors,
        })
    }
}
