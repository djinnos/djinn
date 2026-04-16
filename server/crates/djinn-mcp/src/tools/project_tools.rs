use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::server::DjinnMcpServer;
use djinn_core::auth_context::current_user_token;
use djinn_db::{OrgConfigRepository, ProjectRepository, VerificationRule};

const DJINN_GITIGNORE: &str = "worktrees/\n";

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


// ── Param structs ────────────────────────────────────────────────────────────

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
    /// GitHub App installation id that has access to this repo. When
    /// omitted, the server scans the user's installations and picks one
    /// that contains `owner/repo`.
    #[serde(default)]
    pub installation_id: Option<u64>,
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
    /// GitHub App installation id that surfaced this repo. Pass this back
    /// to [`project_add_from_github`] to pin the clone to the same
    /// installation without re-scanning.
    pub installation_id: u64,
    /// Login of the account (user or org) the installation is scoped to.
    pub account_login: String,
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

        // 1. Must have a session user token from the task-local (set by the
        //    HTTP MCP handler after resolving the `djinn_session` cookie).
        let Some(user_access_token) = current_user_token() else {
            return Json(ProjectAddResponse {
                status: "error: sign in with GitHub required".into(),
                project: ProjectInfo {
                    id: String::new(),
                    name: display_name,
                    path: String::new(),
                },
            });
        };

        // 2. Resolve the installation id — either trust the caller's input
        //    or scan installations to find one that has the repo.
        use djinn_provider::github_app::{
            find_installation_for_repo, get_installation_token,
        };
        let installation_id = if let Some(id) = input.installation_id {
            id
        } else {
            match find_installation_for_repo(&user_access_token, &owner, &repo).await {
                Ok(id) => id,
                Err(e) => {
                    return Json(ProjectAddResponse {
                        status: format!("error: {e}"),
                        project: ProjectInfo {
                            id: String::new(),
                            name: display_name,
                            path: String::new(),
                        },
                    });
                }
            }
        };

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
        //    We mint a fresh 1-hour installation token for the clone URL.
        //    Subsequent `git fetch` calls go through `git_fetch_in`, which
        //    re-uses the cached credential helper only if configured; we
        //    therefore re-request a token per clone attempt rather than
        //    relying on the remote URL embedding a long-lived secret.
        let install_token = match get_installation_token(installation_id).await {
            Ok(t) => t,
            Err(e) => {
                return Json(ProjectAddResponse {
                    status: format!(
                        "error: could not mint installation token for {owner}/{repo}: {e}"
                    ),
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
            install_token.token
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

        // 5b. Configure git user.name/user.email so any commits created by
        //     the server/agents are attributed to the App's bot identity
        //     (`djinn-bot[bot]`). The `<app-id>+djinn-bot[bot]@users.noreply.github.com`
        //     form is GitHub's canonical no-reply email for apps.
        if let Ok(app_id) = djinn_provider::github_app::app_id() {
            let email = format!("{app_id}+djinn-bot[bot]@users.noreply.github.com");
            for (key, value) in [
                ("user.name", "djinn-bot[bot]"),
                ("user.email", email.as_str()),
            ] {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["-C", &clone_path, "config", key, value]);
                if let Err(e) = crate::process::output(cmd).await {
                    tracing::warn!(
                        path = %clone_path, key, error = %e,
                        "project_add_from_github: failed to set git config"
                    );
                }
            }
        } else {
            tracing::warn!(
                "project_add_from_github: GITHUB_APP_ID unset — skipping \
                 djinn-bot[bot] identity config on {}",
                clone_path
            );
        }

        // 6. Seed .djinn/ conveniences.
        let djinn_dir = std::path::Path::new(&clone_path).join(".djinn");
        let _ = fs::create_dir_all(&djinn_dir).await;
        let gitignore_path = djinn_dir.join(".gitignore");
        if !gitignore_path.exists() {
            let _ = fs::write(&gitignore_path, DJINN_GITIGNORE).await;
        }

        // 7. Record the project row (caching the installation id so the push
        //    path doesn't need to rediscover it on every PR create).
        match repo_db
            .create_from_github(
                &display_name,
                &owner,
                &repo,
                &default_branch,
                &clone_path,
                Some(installation_id),
            )
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

    /// List GitHub repositories visible to the deployment's bound
    /// installation (recorded in `org_config`).
    ///
    /// Calls `GET /installation/repositories` with an installation access
    /// token minted from the App JWT + the pinned `installation_id`. No
    /// iteration over App-wide installations — the one-org-per-deployment
    /// invariant means there is exactly one installation to list from.
    #[tool(
        description = "List GitHub repositories accessible via the Djinn App installation bound to this deployment (from org_config). Each entry includes an installation_id and account_login; pass these to project_add_from_github to clone. Populate an Add-Project picker from this tool."
    )]
    pub async fn github_list_repos(
        &self,
        Parameters(input): Parameters<GithubListReposParams>,
    ) -> Json<GithubListReposResponse> {
        use djinn_provider::github_app::{GitHubAppClient, get_installation_by_id};

        if std::env::var("GITHUB_APP_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_none()
        {
            return Json(GithubListReposResponse {
                status: "error: GitHub App not configured".into(),
                repos: vec![],
            });
        }

        // Source of truth: the singleton `org_config` row. If unset, we
        // refuse to list — the deployment has not been bound to a GitHub
        // org yet.
        let org_repo = OrgConfigRepository::new(self.state.db().clone());
        let cfg = match org_repo.get().await {
            Ok(Some(cfg)) => cfg,
            Ok(None) => {
                return Json(GithubListReposResponse {
                    status: "error: deployment not bound to an organization".into(),
                    repos: vec![],
                });
            }
            Err(e) => {
                return Json(GithubListReposResponse {
                    status: format!("error: read org_config: {e}"),
                    repos: vec![],
                });
            }
        };

        let installation_id = cfg.installation_id as u64;

        // Pull the installation's account_login for the response payload.
        // This hits `GET /app/installations/{id}` (App JWT), which is cheap
        // relative to the repo listing call that follows.
        let account_login = match get_installation_by_id(installation_id).await {
            Ok(install) => install.account_login,
            Err(e) => {
                return Json(GithubListReposResponse {
                    status: format!("error: fetch installation {installation_id}: {e}"),
                    repos: vec![],
                });
            }
        };

        let client = GitHubAppClient::new(installation_id);
        let repos = match client.list_repositories(input.per_page).await {
            Ok(r) => r,
            Err(e) => {
                return Json(GithubListReposResponse {
                    status: format!("error: list repositories for installation {installation_id}: {e}"),
                    repos: vec![],
                });
            }
        };

        let out: Vec<GithubRepoEntry> = repos
            .into_iter()
            .map(|r| GithubRepoEntry {
                owner: r.owner,
                repo: r.repo,
                default_branch: r.default_branch,
                private: r.private,
                description: r.description,
                installation_id,
                account_login: account_login.clone(),
            })
            .collect();

        Json(GithubListReposResponse {
            status: "ok".into(),
            repos: out,
        })
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
