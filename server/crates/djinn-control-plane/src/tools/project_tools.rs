use std::path::{Path, PathBuf};

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::server::DjinnMcpServer;
use crate::tools::ObjectJson;
use djinn_core::auth_context::current_user_token;
use djinn_core::models::Project;
use djinn_db::{
    OrgConfigRepository, ProjectConfig, ProjectImage, ProjectImageStatus, ProjectRepository,
    RepoGraphCacheRepository,
};
use djinn_stack::Stack;

/// Build a success-shape `ProjectConfigResponse` from a fully-populated
/// [`ProjectConfig`]. Consolidates the six struct-field copies that
/// used to live in every match arm of `project_config_get` and
/// `project_config_set`.
fn project_config_ok(project_path: &str, config: ProjectConfig) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status: "ok".into(),
        project: project_path.to_owned(),
        target_branch: config.target_branch,
        auto_merge: config.auto_merge,
        sync_enabled: config.sync_enabled,
        sync_remote: config.sync_remote,
        graph_excluded_paths: config.graph_excluded_paths,
        graph_orphan_ignore: config.graph_orphan_ignore,
    }
}

/// Fallback shape used when `get_config` returns `None` (no row) or
/// an error — we still want to echo back the denormalized fields from
/// the `projects` table itself. Graph-exclusion lists aren't in the
/// `Project` struct, so they default to empty (same as a freshly
/// migrated row).
fn project_config_fallback(status: String, project: &Project) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status,
        project: project.path.clone(),
        target_branch: project.target_branch.clone(),
        auto_merge: project.auto_merge,
        sync_enabled: project.sync_enabled,
        sync_remote: project.sync_remote.clone(),
        graph_excluded_paths: Vec::new(),
        graph_orphan_ignore: Vec::new(),
    }
}

/// Error shape used when the project lookup itself fails, so we don't
/// even have a `Project` to echo. Fills the graph-exclusion lists with
/// empty vecs because the caller's form binding expects arrays.
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

const DJINN_GITIGNORE: &str = "worktrees/\n";

/// Resolve the reference-clone root used by `project_add_from_github`.
/// Mirrors `mirrors_root()` in `server/src/server/state/mod.rs`: prefer
/// `$DJINN_HOME/projects` (set by the Helm chart to `/var/lib/djinn/projects`
/// so the non-root `djinn` uid can write there) and fall back to
/// `~/.djinn/projects` for local/docker-compose runs where HOME is `/root`.
fn projects_root() -> PathBuf {
    if let Ok(djinn_home) = std::env::var("DJINN_HOME")
        && !djinn_home.is_empty()
    {
        return PathBuf::from(djinn_home).join("projects");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".djinn")
        .join("projects")
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
    pub installation_id: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GithubListReposParams {
    /// Max number of repositories to return (1..=100). Defaults to 30.
    #[serde(default)]
    pub per_page: Option<i64>,
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
    pub installation_id: i64,
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

#[derive(Deserialize, JsonSchema)]
pub struct GetProjectStackParams {
    /// Project UUID whose detected stack should be returned.
    pub project: String,
}

#[derive(Serialize, JsonSchema)]
pub struct GetProjectStackResponse {
    /// Detected stack metadata, or `None` when the project exists but no
    /// detection has run yet (default `{}` in the DB) or when the
    /// persisted JSON fails to deserialize.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<Stack>,
    /// Populated on lookup / deserialization failures; clients should
    /// surface this verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetProjectDevcontainerStatusParams {
    /// Project UUID whose devcontainer + image status should be returned.
    pub project: String,
}

/// Snapshot of a project's image-build state, used by the UI onboarding
/// banner.
#[derive(Serialize, JsonSchema)]
pub struct GetProjectDevcontainerStatusResponse {
    /// Content-addressable image tag from the last successful build, or
    /// `None` when no build has completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
    /// One of `none | building | ready | failed`.
    pub image_status: String,
    /// Human-readable error from the most recent failed build, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_last_error: Option<String>,
    /// ISO-8601 UTC timestamp of the most recent successful canonical-graph
    /// warm for this project. `None` means the warmer has not completed a
    /// run yet (cold project or failing pipeline). The coordinator will not
    /// dispatch tasks until this is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_warmed_at: Option<String>,
    /// Derived status for the UI banner. One of
    /// `pending | running | ready | failed`. `pending` means no warm has
    /// ever run; `running` means the image is ready and a warm should be
    /// in flight (or imminent); `ready` means `graph_warmed_at` is set;
    /// `failed` mirrors the image build's failed status (no warm possible).
    pub graph_warm_status: String,
    /// Populated on lookup failures; clients should surface this verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RetriggerImageBuildParams {
    /// Project UUID whose image should be rebuilt on the next mirror-fetch tick.
    pub project: String,
}

#[derive(Serialize, JsonSchema)]
pub struct RetriggerImageBuildResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigGetParams {
    /// Project UUID.
    pub project: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigGetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The raw JSON config currently in `projects.environment_config`.
    /// Empty object `{}` when the row hasn't been reseeded yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ObjectJson>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigSetParams {
    /// Project UUID.
    pub project: String,
    /// Full `EnvironmentConfig` JSON blob. Validated server-side via
    /// `djinn_stack::environment::EnvironmentConfig::validate` before
    /// anything is written.
    pub config: ObjectJson,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigSetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigResetParams {
    /// Project UUID.
    pub project: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigResetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The freshly-generated auto-detected config, on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<ObjectJson>,
}

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

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Sort local branches alphabetically and hoist `current` (if any) to the front.
fn order_branches(mut branches: Vec<String>, current: Option<&str>) -> Vec<String> {
    branches.sort();
    branches.dedup();
    if let Some(cur) = current
        && let Some(pos) = branches.iter().position(|b| b == cur)
    {
        let c = branches.remove(pos);
        branches.insert(0, c);
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

/// Derive the banner's `graph_warm_status` from image status + warm stamp.
fn derive_graph_warm_status(image_status: &str, graph_warmed_at: &Option<String>) -> String {
    if graph_warmed_at.is_some() {
        return "ready".to_string();
    }
    match image_status {
        s if s == ProjectImageStatus::FAILED => "failed".to_string(),
        s if s == ProjectImageStatus::READY => "running".to_string(),
        _ => "pending".to_string(),
    }
}

/// Build a `GetProjectDevcontainerStatusResponse` for the error/early-exit
/// paths. Keeps the many early-return sites short + consistent.
fn error_response(
    image_status: String,
    image_last_error: Option<String>,
    error: String,
) -> GetProjectDevcontainerStatusResponse {
    let graph_warm_status = derive_graph_warm_status(&image_status, &None);
    GetProjectDevcontainerStatusResponse {
        image_tag: None,
        image_status,
        image_last_error,
        graph_warmed_at: None,
        graph_warm_status,
        error: Some(error),
    }
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
        description = "Add a project by cloning a GitHub repo the Djinn App can access. The server clones into $DJINN_HOME/projects/{owner}/{repo} (Helm mounts this at /var/lib/djinn/projects; docker-compose falls back to ~/.djinn/projects). Idempotent: re-adding runs `git fetch` instead of cloning again."
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
        let display_name = input.name.unwrap_or_else(|| repo.clone());

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
        use djinn_provider::github_app::{find_installation_for_repo, get_installation_token};
        let installation_id: u64 = if let Some(id) = input.installation_id {
            id.max(0) as u64
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

        let default_branch = input.git_ref.clone().unwrap_or_else(|| "main".to_string());

        // 3. Choose clone_path under the server-managed projects root.
        let clone_path = projects_root()
            .join(&owner)
            .join(&repo)
            .to_string_lossy()
            .into_owned();

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
            cmd.args(["clone", "--filter=blob:none", &remote_url, &clone_path]);
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

        // Source of truth: the singleton `org_config` row written by the
        // in-UI installation picker.
        let installation_id = {
            let org_repo = OrgConfigRepository::new(self.state.db().clone());
            match org_repo.get().await {
                Ok(Some(cfg)) => cfg.installation_id as u64,
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
            }
        };

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
        let per_page_usize: Option<usize> = input.per_page.map(|n| n.clamp(1, 100) as usize);
        let repos = match client.list_repositories(per_page_usize).await {
            Ok(r) => r,
            Err(e) => {
                return Json(GithubListReposResponse {
                    status: format!(
                        "error: list repositories for installation {installation_id}: {e}"
                    ),
                    repos: vec![],
                });
            }
        };

        let installation_id_i64: i64 = installation_id as i64;
        let out: Vec<GithubRepoEntry> = repos
            .into_iter()
            .map(|r| GithubRepoEntry {
                owner: r.owner,
                repo: r.repo,
                default_branch: r.default_branch,
                private: r.private,
                description: r.description,
                installation_id: installation_id_i64,
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
            Ok(Some(config)) => Json(project_config_ok(&project.path, config)),
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
        let project = match repo.get_by_path(&input.project).await {
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
            Ok(Some(config)) => Json(project_config_ok(&project.path, config)),
            Ok(None) => Json(project_config_fallback(
                format!("error: invalid key '{}'", input.key),
                &project,
            )),
            Err(e) => Json(project_config_fallback(format!("error: {e}"), &project)),
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
        head_cmd.args(["-C", &path, "rev-parse", "--abbrev-ref", "HEAD"]);
        let head_fut = crate::process::output(head_cmd);
        let head_out =
            match tokio::time::timeout(std::time::Duration::from_secs(30), head_fut).await {
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
        list_cmd.args(["-C", &path, "branch", "--list", "--format=%(refname:short)"]);
        let list_fut = crate::process::output(list_cmd);
        let list_out =
            match tokio::time::timeout(std::time::Duration::from_secs(30), list_fut).await {
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

    /// Return the detected stack metadata for a project, as populated by
    /// the mirror-fetcher hook after each successful fetch. The empty JSON
    /// default (`{}`) surfaces as `stack: None`.
    #[tool(
        description = "Return detected stack metadata for a project (languages, package managers, frameworks, devcontainer status)."
    )]
    pub async fn get_project_stack(
        &self,
        Parameters(input): Parameters<GetProjectStackParams>,
    ) -> Json<GetProjectStackResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.get_stack(&input.project).await {
            Ok(Some(raw)) => {
                if raw.trim() == "{}" || raw.trim().is_empty() {
                    return Json(GetProjectStackResponse {
                        stack: None,
                        error: None,
                    });
                }
                match serde_json::from_str::<Stack>(&raw) {
                    Ok(stack) => Json(GetProjectStackResponse {
                        stack: Some(stack),
                        error: None,
                    }),
                    Err(err) => Json(GetProjectStackResponse {
                        stack: None,
                        error: Some(format!("stack JSON deserialize failed: {err}")),
                    }),
                }
            }
            Ok(None) => Json(GetProjectStackResponse {
                stack: None,
                error: Some(format!("project not found: {}", input.project)),
            }),
            Err(err) => Json(GetProjectStackResponse {
                stack: None,
                error: Some(format!("stack lookup failed: {err}")),
            }),
        }
    }

    /// Return image-build status for a project.
    ///
    /// Drives the UI status badge (P7). Joins the `image_*` columns with
    /// the `graph_warmed_at` stamp so the badge can reflect both build
    /// progress and canonical-graph warm readiness.
    #[tool(
        description = "Return image-build status for a project (image_tag, image_status, image_last_error, graph_warmed_at, graph_warm_status). Drives the UI status badge."
    )]
    pub async fn get_project_devcontainer_status(
        &self,
        Parameters(input): Parameters<GetProjectDevcontainerStatusParams>,
    ) -> Json<GetProjectDevcontainerStatusResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // Existence check — if the project id is unknown, surface it up
        // front so the badge doesn't render "building…" indefinitely.
        if let Err(err) = repo.get_stack(&input.project).await {
            return Json(error_response(
                ProjectImageStatus::NONE.to_string(),
                None,
                format!("stack lookup failed: {err}"),
            ));
        }

        let image = match repo.get_project_image(&input.project).await {
            Ok(Some(img)) => img,
            Ok(None) => ProjectImage::none(),
            Err(err) => {
                return Json(error_response(
                    ProjectImageStatus::NONE.to_string(),
                    None,
                    format!("image state lookup failed: {err}"),
                ));
            }
        };

        // Graph-warm status: derived from the dispatch-readiness row so the
        // banner can surface a distinct progress row alongside image state.
        // Errors are swallowed — banner shows `pending` on lookup failure.
        //
        // Fall back to `repo_graph_cache.built_at` when `projects.graph_warmed_at`
        // is missing: the stamp is best-effort (the cache upsert logs a warning
        // and continues on failure), and rows written before migration 9
        // landed never got stamped. Treating a present cache row as "warmed"
        // keeps the banner honest when the two sources drift.
        let stamp_from_project = repo
            .get_dispatch_readiness(&input.project)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.graph_warmed_at);

        let graph_warmed_at = match stamp_from_project {
            Some(v) => Some(v),
            None => RepoGraphCacheRepository::new(self.state.db().clone())
                .latest_for_project(&input.project)
                .await
                .ok()
                .flatten()
                .map(|row| row.built_at),
        };

        let graph_warm_status = derive_graph_warm_status(&image.status, &graph_warmed_at);

        Json(GetProjectDevcontainerStatusResponse {
            image_tag: image.tag,
            image_status: image.status,
            image_last_error: image.last_error,
            graph_warmed_at,
            graph_warm_status,
            error: None,
        })
    }

    /// Force the image controller to rebuild the project's image on the
    /// next mirror-fetch tick.
    ///
    /// Nulls `projects.image_hash` so the controller's unchanged-hash
    /// fast-path is defeated; the next `enqueue(project_id, stack)` call
    /// recomputes from HEAD and submits a build Job. Status is flipped to
    /// `building` so the banner reflects the pending rebuild immediately
    /// without waiting for the mirror-fetcher cadence.
    #[tool(
        description = "Mark a project's image for rebuild on the next mirror-fetch tick. Nulls the cached devcontainer hash so the image controller re-enqueues a build."
    )]
    pub async fn retrigger_image_build(
        &self,
        Parameters(input): Parameters<RetriggerImageBuildParams>,
    ) -> Json<RetriggerImageBuildResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // Load the current image record so we don't clobber tag / error.
        let mut image = match repo.get_project_image(&input.project).await {
            Ok(Some(img)) => img,
            Ok(None) => {
                return Json(RetriggerImageBuildResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                });
            }
            Err(err) => {
                return Json(RetriggerImageBuildResponse {
                    status: "error".into(),
                    error: Some(format!("image state lookup failed: {err}")),
                });
            }
        };

        // Clear hash + error, flip to building. The controller's next
        // enqueue recomputes the hash from HEAD and submits a Job.
        image.hash = None;
        image.last_error = None;
        image.status = ProjectImageStatus::BUILDING.to_string();

        match repo.set_project_image(&input.project, &image).await {
            Ok(()) => Json(RetriggerImageBuildResponse {
                status: "ok".into(),
                error: None,
            }),
            Err(err) => Json(RetriggerImageBuildResponse {
                status: "error".into(),
                error: Some(format!("failed to flag image for rebuild: {err}")),
            }),
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
                Json(ProjectEnvironmentConfigGetResponse {
                    status: "ok".into(),
                    error: None,
                    config: Some(ObjectJson::from(parsed)),
                })
            }
            Ok(None) => Json(ProjectEnvironmentConfigGetResponse {
                status: "error".into(),
                error: Some(format!("project not found: {}", input.project)),
                config: None,
            }),
            Err(err) => Json(ProjectEnvironmentConfigGetResponse {
                status: "error".into(),
                error: Some(format!("db error: {err}")),
                config: None,
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
        let cfg: djinn_stack::environment::EnvironmentConfig = match serde_json::from_value(
            serde_json::Value::Object(input.config.0),
        ) {
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
            Ok(serde_json::Value::Object(map)) => Some(ObjectJson::from(serde_json::Value::Object(map))),
            _ => None,
        };
        Json(ProjectEnvironmentConfigResetResponse {
            status: "ok".into(),
            error: None,
            config: json,
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
