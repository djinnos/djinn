use std::path::Path;
use std::sync::Arc;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::server::DjinnMcpServer;
use djinn_db::{ProjectRepository, VerificationRule};
use djinn_provider::github_api::GitHubApiClient;
use djinn_provider::oauth::github_app::GitHubAppTokens;
use djinn_provider::repos::CredentialRepository;

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

/// Run `git fetch --all --prune` inside `path`. Best-effort refresh for an
/// existing server-managed clone.
async fn git_fetch_in(path: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["fetch", "--all", "--prune"]).current_dir(path);
    let output = crate::process::output(cmd)
        .await
        .map_err(|e| format!("git fetch failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git fetch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Parse `owner` and `repo` from a GitHub remote URL.
///
/// Supports both HTTPS (`https://github.com/owner/repo.git`) and SSH
/// (`git@github.com:owner/repo.git`) formats.
fn parse_github_owner_repo(remote_url: &str) -> Option<(String, String)> {
    // Normalize: strip user@ from HTTPS URLs (e.g. https://user@github.com/...)
    let url = if let Some(rest) = remote_url.strip_prefix("https://") {
        if let Some(at_pos) = rest.find('@') {
            format!("https://{}", &rest[at_pos + 1..])
        } else {
            remote_url.to_string()
        }
    } else if let Some(rest) = remote_url.strip_prefix("http://") {
        if let Some(at_pos) = rest.find('@') {
            format!("http://{}", &rest[at_pos + 1..])
        } else {
            remote_url.to_string()
        }
    } else {
        remote_url.to_string()
    };

    // SSH: git@github.com:owner/repo.git
    if let Some(path) = url.strip_prefix("git@github.com:") {
        return split_owner_repo(path);
    }
    // HTTPS: https://github.com/owner/repo.git or http://
    for prefix in &["https://github.com/", "http://github.com/"] {
        if let Some(path) = url.strip_prefix(prefix) {
            return split_owner_repo(path);
        }
    }
    None
}

fn split_owner_repo(path: &str) -> Option<(String, String)> {
    let path = path.trim_end_matches(".git");
    let mut parts = path.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

/// Validate that the project has a GitHub origin remote and that the Djinn
/// GitHub App can access the repository.
///
/// Returns `Ok((owner, repo))` on success or `Err(message)` with a
/// user-facing error.
async fn validate_github_remote(
    path: &str,
    cred_repo: Arc<CredentialRepository>,
) -> Result<(String, String), String> {
    // 1. Read the origin remote URL.
    let project_path = std::path::PathBuf::from(path);
    let mut cmd = std::process::Command::new("git");
    cmd.args(["remote", "get-url", "origin"])
        .current_dir(&project_path);
    let output = crate::process::output(cmd)
        .await
        .map_err(|e| format!("git remote get-url origin failed: {e}"))?;

    if !output.status.success() {
        return Err(
            "Project must have a GitHub remote. Add one with `git remote add origin git@github.com:owner/repo.git`"
                .to_string(),
        );
    }

    let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // 2. Parse owner/repo from the URL.
    let (owner, repo) = parse_github_owner_repo(&remote_url).ok_or_else(|| {
        "Project must have a GitHub remote. Add one with `git remote add origin git@github.com:owner/repo.git`"
            .to_string()
    })?;

    // 3. Check that we have GitHub tokens.
    let has_tokens = GitHubAppTokens::load_from_db(&cred_repo).await.is_some();
    if !has_tokens {
        return Err("Connect GitHub first".to_string());
    }

    // 4. Validate the GitHub App can access the repo (best-effort).
    // This may fail if the installation token cannot be derived yet (e.g. app
    // not installed on the org). We log a warning but still allow the project
    // to be added — the board_health check will surface the issue later.
    let github_client = GitHubApiClient::new(cred_repo);
    match github_client.check_repo_access(&owner, &repo).await {
        Ok(()) => {
            tracing::info!(owner = %owner, repo = %repo, "project_add: GitHub access verified");
        }
        Err(e) => {
            tracing::warn!(
                owner = %owner,
                repo = %repo,
                error = %e,
                "project_add: GitHub repo access check failed — install the Djinn app on {owner} at https://github.com/apps/djinn-ai-bot/installations/new",
            );
        }
    }

    Ok((owner, repo))
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

#[derive(Deserialize, JsonSchema)]
pub struct ProjectAddFromGithubParams {
    /// GitHub owner (user or organization).
    pub owner: String,
    /// GitHub repository name.
    pub repo: String,
    /// Optional project display name. Defaults to `{owner}/{repo}`.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional branch to check out after cloning. Defaults to the repo's
    /// default branch as reported by the GitHub API.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GithubListReposParams {
    /// Max number of repositories to return (1..=100). Defaults to 30.
    #[serde(default)]
    pub per_page: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct GithubRepoEntry {
    pub owner: String,
    pub repo: String,
    pub default_branch: String,
    pub private: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct GithubListReposResponse {
    pub status: String,
    pub repos: Vec<GithubRepoEntry>,
}

// ── Response structs ─────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProjectConfigGetParams {
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectBranchesParams {
    /// Project UUID to resolve the server-owned clone path for.
    pub project_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectBranchesResponse {
    pub status: String,
    pub branches: Vec<String>,
    pub current: Option<String>,
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

/// Sort local branches alphabetically and hoist `current` (if any) to the front.
fn order_branches(mut branches: Vec<String>, current: Option<&str>) -> Vec<String> {
    branches.sort();
    branches.dedup();
    if let Some(cur) = current {
        if let Some(pos) = branches.iter().position(|b| b == cur) {
            let c = branches.remove(pos);
            branches.insert(0, c);
        }
    }
    branches
}

/// Parse the output of `git branch --list --format=%(refname:short)` into a
/// clean `Vec<String>`. Empty lines and lines starting with `(` (detached
/// HEAD marker) are skipped.
fn parse_branch_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('('))
        .map(|l| l.to_string())
        .collect()
}

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

        // Validate GitHub remote: origin must point to a GitHub repo the App can access.
        let cred_repo = Arc::new(CredentialRepository::new(
            self.state.db().clone(),
            self.state.event_bus(),
        ));
        if let Err(msg) = validate_github_remote(path, cred_repo).await {
            return Json(ProjectAddResponse {
                status: format!("error: {msg}"),
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

    /// Register a project by cloning a GitHub repo into the server's
    /// managed storage. Supersedes `project_add` for the Docker-hosted
    /// deployment where the host filesystem is not visible to the server.
    #[tool(
        description = "Add a project by cloning a GitHub repo the Djinn App can access. The server clones into /root/.djinn/projects/{owner}/{repo} (persisted on the host via the ~/.djinn bind mount). Idempotent: re-adding runs `git fetch` instead of cloning again."
    )]
    pub async fn project_add_from_github(
        &self,
        Parameters(input): Parameters<ProjectAddFromGithubParams>,
    ) -> Json<ProjectAddResponse> {
        let repo_db = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let cred_repo = Arc::new(CredentialRepository::new(
            self.state.db().clone(),
            self.state.event_bus(),
        ));

        let owner = input.owner.trim().to_string();
        let repo = input.repo.trim().to_string();
        if owner.is_empty() || repo.is_empty() {
            return Json(ProjectAddResponse {
                status: "error: owner and repo must be non-empty".into(),
                project: ProjectInfo {
                    id: String::new(),
                    name: input.name.unwrap_or_default(),
                    path: String::new(),
                },
            });
        }
        let display_name = input
            .name
            .unwrap_or_else(|| format!("{owner}/{repo}"));

        // 1. Must have GitHub App tokens loaded.
        if GitHubAppTokens::load_from_db(&cred_repo).await.is_none() {
            return Json(ProjectAddResponse {
                status: "error: Connect GitHub first".into(),
                project: ProjectInfo {
                    id: String::new(),
                    name: display_name,
                    path: String::new(),
                },
            });
        }

        // 2. Verify App access + discover default branch via GitHub REST.
        let client = GitHubApiClient::new(cred_repo.clone());
        if let Err(e) = client.check_repo_access(&owner, &repo).await {
            return Json(ProjectAddResponse {
                status: format!(
                    "error: GitHub App cannot access {owner}/{repo}: {e}. \
                     Install the Djinn app at https://github.com/apps/djinn-ai-bot/installations/new",
                ),
                project: ProjectInfo {
                    id: String::new(),
                    name: display_name,
                    path: String::new(),
                },
            });
        }

        let default_branch = input
            .git_ref
            .clone()
            .unwrap_or_else(|| "main".to_string());

        // 3. Choose clone_path under the server-managed projects root.
        let clone_path = format!("/root/.djinn/projects/{owner}/{repo}");

        // Idempotent: if already registered, fast-path to `git fetch`.
        if let Ok(Some(existing)) = repo_db.get_by_github(&owner, &repo).await {
            let _ = fs::create_dir_all(&existing.path).await;
            if let Err(e) = git_fetch_in(&existing.path).await {
                tracing::warn!(
                    owner = %owner, repo = %repo, error = %e,
                    "project_add_from_github: fetch refresh failed",
                );
            }
            return Json(ProjectAddResponse {
                status: "ok".into(),
                project: ProjectInfo {
                    id: existing.id,
                    name: existing.name,
                    path: existing.path,
                },
            });
        }

        // 4. Ensure parent dir exists.
        if let Some(parent) = std::path::Path::new(&clone_path).parent() {
            let _ = fs::create_dir_all(parent).await;
        }

        // 5. Shallow-ish clone (blob filter keeps history light).
        //    Token auth via https URL keeps this self-contained; the token
        //    lifecycle is the same one used by all REST calls.
        let tokens = match GitHubAppTokens::load_from_db(&cred_repo).await {
            Some(t) => t,
            None => {
                return Json(ProjectAddResponse {
                    status: "error: GitHub token vanished between checks".into(),
                    project: ProjectInfo {
                        id: String::new(),
                        name: display_name,
                        path: clone_path,
                    },
                });
            }
        };
        let remote_url = format!(
            "https://x-access-token:{}@github.com/{owner}/{repo}.git",
            tokens.access_token
        );

        if !std::path::Path::new(&clone_path).join(".git").exists() {
            let mut cmd = std::process::Command::new("git");
            cmd.args([
                "clone",
                "--filter=blob:none",
                &remote_url,
                &clone_path,
            ]);
            let output = match crate::process::output(cmd).await {
                Ok(o) => o,
                Err(e) => {
                    return Json(ProjectAddResponse {
                        status: format!("error: git clone failed: {e}"),
                        project: ProjectInfo {
                            id: String::new(),
                            name: display_name,
                            path: clone_path,
                        },
                    });
                }
            };
            if !output.status.success() {
                return Json(ProjectAddResponse {
                    status: format!(
                        "error: git clone failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                    project: ProjectInfo {
                        id: String::new(),
                        name: display_name,
                        path: clone_path,
                    },
                });
            }
        } else {
            // Directory already present from a previous partial add — refresh it.
            if let Err(e) = git_fetch_in(&clone_path).await {
                tracing::warn!(path = %clone_path, error = %e, "pre-existing clone fetch failed");
            }
        }

        // 6. Seed .djinn/ conveniences.
        let djinn_dir = std::path::Path::new(&clone_path).join(".djinn");
        let _ = fs::create_dir_all(&djinn_dir).await;
        let gitignore_path = djinn_dir.join(".gitignore");
        if !gitignore_path.exists() {
            let _ = fs::write(&gitignore_path, DJINN_GITIGNORE).await;
        }

        // 7. Record the project row.
        match repo_db
            .create_from_github(&display_name, &owner, &repo, &default_branch, &clone_path)
            .await
        {
            Ok(project) => Json(ProjectAddResponse {
                status: "ok".into(),
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
                    name: display_name,
                    path: clone_path,
                },
            }),
        }
    }

    /// List GitHub repositories the authenticated GitHub App user can access,
    /// sorted by most-recently-pushed. Intended for the desktop UI picker.
    #[tool(
        description = "List GitHub repositories the Djinn App can access (sorted by recent activity). Use to populate an Add-Project picker."
    )]
    pub async fn github_list_repos(
        &self,
        Parameters(input): Parameters<GithubListReposParams>,
    ) -> Json<GithubListReposResponse> {
        let cred_repo = Arc::new(CredentialRepository::new(
            self.state.db().clone(),
            self.state.event_bus(),
        ));
        if GitHubAppTokens::load_from_db(&cred_repo).await.is_none() {
            return Json(GithubListReposResponse {
                status: "error: Connect GitHub first".into(),
                repos: vec![],
            });
        }
        let client = GitHubApiClient::new(cred_repo);
        match client.list_user_repos(input.per_page).await {
            Ok(entries) => Json(GithubListReposResponse {
                status: "ok".into(),
                repos: entries
                    .into_iter()
                    .map(|e| GithubRepoEntry {
                        owner: e.owner,
                        repo: e.repo,
                        default_branch: e.default_branch,
                        private: e.private,
                        description: e.description,
                    })
                    .collect(),
            }),
            Err(e) => Json(GithubListReposResponse {
                status: format!("error: {e}"),
                repos: vec![],
            }),
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
    /// List local git branches in a project's server-owned clone.
    #[tool(
        description = "List local git branches in the server-owned clone for a project. Returns branches sorted alphabetically with the currently checked-out branch first."
    )]
    pub async fn project_branches(
        &self,
        Parameters(input): Parameters<ProjectBranchesParams>,
    ) -> Json<ProjectBranchesResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        let project = match repo.get(&input.project_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Json(ProjectBranchesResponse {
                    status: format!("error: project not found: {}", input.project_id),
                    branches: vec![],
                    current: None,
                });
            }
            Err(e) => {
                return Json(ProjectBranchesResponse {
                    status: format!("error: {e}"),
                    branches: vec![],
                    current: None,
                });
            }
        };

        // `path` is set equal to `clone_path` for github-cloned rows and to the
        // user-supplied path for legacy rows, so it's the right column either way.
        let path = project.path;
        if !Path::new(&path).join(".git").exists() {
            return Json(ProjectBranchesResponse {
                status: format!("error: not a git repository: {path}"),
                branches: vec![],
                current: None,
            });
        }

        // 1. Current branch via `git rev-parse --abbrev-ref HEAD`.
        let mut head_cmd = std::process::Command::new("git");
        head_cmd
            .args(["-C", &path, "rev-parse", "--abbrev-ref", "HEAD"]);
        let head_fut = crate::process::output(head_cmd);
        let head_out = match tokio::time::timeout(std::time::Duration::from_secs(30), head_fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Json(ProjectBranchesResponse {
                    status: format!("error: git rev-parse failed: {e}"),
                    branches: vec![],
                    current: None,
                });
            }
            Err(_) => {
                return Json(ProjectBranchesResponse {
                    status: "error: git rev-parse timed out after 30s".into(),
                    branches: vec![],
                    current: None,
                });
            }
        };
        let current = if head_out.status.success() {
            let raw = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
            // Detached HEAD surfaces as "HEAD" — treat as no current branch.
            if raw.is_empty() || raw == "HEAD" {
                None
            } else {
                Some(raw)
            }
        } else {
            None
        };

        // 2. Local branch list.
        let mut list_cmd = std::process::Command::new("git");
        list_cmd.args([
            "-C",
            &path,
            "branch",
            "--list",
            "--format=%(refname:short)",
        ]);
        let list_fut = crate::process::output(list_cmd);
        let list_out = match tokio::time::timeout(std::time::Duration::from_secs(30), list_fut).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Json(ProjectBranchesResponse {
                    status: format!("error: git branch failed: {e}"),
                    branches: vec![],
                    current,
                });
            }
            Err(_) => {
                return Json(ProjectBranchesResponse {
                    status: "error: git branch timed out after 30s".into(),
                    branches: vec![],
                    current,
                });
            }
        };
        if !list_out.status.success() {
            return Json(ProjectBranchesResponse {
                status: format!(
                    "error: git branch failed: {}",
                    String::from_utf8_lossy(&list_out.stderr).trim()
                ),
                branches: vec![],
                current,
            });
        }

        let stdout = String::from_utf8_lossy(&list_out.stdout);
        let parsed = parse_branch_list(&stdout);
        let branches = order_branches(parsed, current.as_deref());

        Json(ProjectBranchesResponse {
            status: "ok".into(),
            branches,
            current,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_remote() {
        let (owner, repo) = parse_github_owner_repo("git@github.com:acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_remote_with_dot_git() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_remote_without_dot_git() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_http_remote() {
        let (owner, repo) = parse_github_owner_repo("http://github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_non_github_remote_returns_none() {
        assert!(parse_github_owner_repo("git@gitlab.com:acme/widgets.git").is_none());
        assert!(parse_github_owner_repo("https://gitlab.com/acme/widgets.git").is_none());
    }

    #[test]
    fn parse_empty_owner_or_repo_returns_none() {
        assert!(parse_github_owner_repo("git@github.com:/widgets.git").is_none());
        assert!(parse_github_owner_repo("git@github.com:acme/").is_none());
    }

    #[test]
    fn parse_https_with_user_prefix() {
        let (owner, repo) = parse_github_owner_repo(
            "https://CroosALt@github.com/getalternative/svc-accounts-payable.git",
        )
        .unwrap();
        assert_eq!(owner, "getalternative");
        assert_eq!(repo, "svc-accounts-payable");
    }

    #[test]
    fn parse_https_with_user_prefix_no_dot_git() {
        let (owner, repo) =
            parse_github_owner_repo("https://user@github.com/acme/widgets").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_http_with_user_prefix() {
        let (owner, repo) =
            parse_github_owner_repo("http://user@github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_non_github_with_user_prefix_returns_none() {
        assert!(parse_github_owner_repo("https://user@gitlab.com/acme/widgets.git").is_none());
    }

    #[test]
    fn parse_branch_list_skips_empty_and_detached_marker() {
        let raw = "main\n\nfeature/x\n(HEAD detached at abc123)\nrelease/1.0\n";
        let parsed = parse_branch_list(raw);
        assert_eq!(parsed, vec!["main", "feature/x", "release/1.0"]);
    }

    #[test]
    fn order_branches_hoists_current_and_sorts() {
        let branches = vec![
            "release/1.0".to_string(),
            "main".to_string(),
            "feature/x".to_string(),
        ];
        let ordered = order_branches(branches, Some("feature/x"));
        assert_eq!(ordered, vec!["feature/x", "main", "release/1.0"]);
    }

    #[test]
    fn order_branches_without_current_just_sorts() {
        let branches = vec!["b".to_string(), "a".to_string(), "c".to_string()];
        let ordered = order_branches(branches, None);
        assert_eq!(ordered, vec!["a", "b", "c"]);
    }

    #[test]
    fn order_branches_current_not_in_list_is_noop() {
        let branches = vec!["a".to_string(), "b".to_string()];
        let ordered = order_branches(branches, Some("missing"));
        assert_eq!(ordered, vec!["a", "b"]);
    }
}
