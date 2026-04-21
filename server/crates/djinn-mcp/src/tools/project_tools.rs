use std::path::{Path, PathBuf};

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::server::DjinnMcpServer;
use djinn_core::auth_context::current_user_token;
use djinn_db::{
    OrgConfigRepository, ProjectImage, ProjectImageStatus, ProjectRepository,
    RepoGraphCacheRepository, VerificationRule,
};
use djinn_provider::github_api::{CreatePrParams, GitHubApiClient};
use djinn_stack::{Stack, generate_starter};

/// Stable branch name used for the one-click devcontainer PR. Reusing the
/// same ref across attempts keeps PR creation idempotent — a second click
/// after the first PR was closed without merging is handled by returning
/// the still-open PR (if any) or updating the file on the existing branch.
const DEVCONTAINER_PR_BRANCH: &str = "djinn/setup-devcontainer";
const DEVCONTAINER_PATH: &str = ".devcontainer/devcontainer.json";

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

/// Snapshot of a project's devcontainer + image-build state, used by the
/// UI onboarding banner.
#[derive(Serialize, JsonSchema)]
pub struct GetProjectDevcontainerStatusResponse {
    /// True when the mirror's HEAD contains `.devcontainer/devcontainer.json`.
    pub has_devcontainer: bool,
    /// True when the mirror's HEAD also contains `.devcontainer/devcontainer-lock.json`.
    pub has_devcontainer_lock: bool,
    /// Content-addressable image tag from the last successful build, or
    /// `None` when no build has completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
    /// One of `none | building | ready | failed`.
    pub image_status: String,
    /// Human-readable error from the most recent failed build, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_last_error: Option<String>,
    /// Generated starter `devcontainer.json` (pretty-printed). Populated
    /// when `has_devcontainer == false` so the UI can show a copyable
    /// template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starter_json: Option<String>,
    /// Reference to an already-open PR on the `djinn/setup-devcontainer`
    /// branch, when one exists. The UI uses this to swap the "Open PR"
    /// button to "View PR" without a second round-trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_setup_pr: Option<DevcontainerPrRef>,
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

/// Minimal reference to a devcontainer setup PR — enough for the UI to
/// render a "View PR" link and navigate the user to GitHub.
#[derive(Serialize, JsonSchema)]
pub struct DevcontainerPrRef {
    /// Full `html_url` of the PR on GitHub.
    pub url: String,
    /// PR number within the repo.
    #[schemars(with = "i64")]
    pub number: u64,
}

#[derive(Deserialize, JsonSchema)]
pub struct DevcontainerOpenPrParams {
    /// Project UUID to open a devcontainer PR on.
    pub project: String,
}

#[derive(Serialize, JsonSchema)]
pub struct DevcontainerOpenPrResponse {
    /// PR that was opened or is already open. `None` only on error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<DevcontainerPrRef>,
    /// True when a PR with the setup head branch was already open prior
    /// to this call — the server did not push a new commit.
    pub already_open: bool,
    /// Populated on any failure along the path (missing coords, API
    /// error, etc.); the client surfaces this verbatim.
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
        has_devcontainer: false,
        has_devcontainer_lock: false,
        image_tag: None,
        image_status,
        image_last_error,
        starter_json: None,
        open_setup_pr: None,
        graph_warmed_at: None,
        graph_warm_status,
        error: Some(error),
    }
}

/// Look up an open PR on the `djinn/setup-devcontainer` branch for a
/// project. Silently returns `None` when the project has no GitHub coords,
/// no cached installation id, or when the GitHub API call fails — the
/// caller treats this as "no PR open, show the create button".
async fn find_open_setup_pr(
    repo: &ProjectRepository,
    project_id: &str,
) -> Option<DevcontainerPrRef> {
    let (owner, repo_name) = repo.get_github_coords(project_id).await.ok()??;
    let installation_id = repo.get_installation_id(project_id).await.ok()??;

    let client = GitHubApiClient::for_installation(installation_id);
    let head = format!("{owner}:{DEVCONTAINER_PR_BRANCH}");
    let prs = client
        .list_pulls_by_head_with_state(&owner, &repo_name, &head, "open")
        .await
        .ok()?;
    prs.into_iter().next().map(|pr| DevcontainerPrRef {
        url: pr.html_url,
        number: pr.number,
    })
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

    /// Return devcontainer + image-build status for a project.
    ///
    /// Drives the UI onboarding banner (Phase 3 PR 6). Reads the latest
    /// stack JSON for the two `manifest_signals.has_devcontainer*` flags,
    /// joins it with the `image_*` columns, and — when the project has
    /// no committed devcontainer — fills `starter_json` with
    /// `djinn_stack::generate_starter(&stack)` so the banner can show a
    /// copyable template.
    #[tool(
        description = "Return devcontainer + image-build status for a project (has_devcontainer, has_devcontainer_lock, image_tag, image_status, image_last_error, starter_json). Drives the UI onboarding banner."
    )]
    pub async fn get_project_devcontainer_status(
        &self,
        Parameters(input): Parameters<GetProjectDevcontainerStatusParams>,
    ) -> Json<GetProjectDevcontainerStatusResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // Resolve the stack first — the `has_devcontainer*` signals drive
        // whether we bother generating a starter template.
        let stack_raw = match repo.get_stack(&input.project).await {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                return Json(error_response(
                    ProjectImageStatus::NONE.to_string(),
                    None,
                    format!("project not found: {}", input.project),
                ));
            }
            Err(err) => {
                return Json(error_response(
                    ProjectImageStatus::NONE.to_string(),
                    None,
                    format!("stack lookup failed: {err}"),
                ));
            }
        };

        // Treat empty / default (`{}`) stack as "not detected yet" — no
        // signals known, no starter JSON; banner will render the
        // `missing` state as soon as detection runs.
        let stack: Option<Stack> = if stack_raw.trim().is_empty() || stack_raw.trim() == "{}" {
            None
        } else {
            match serde_json::from_str::<Stack>(&stack_raw) {
                Ok(s) => Some(s),
                Err(err) => {
                    return Json(error_response(
                        ProjectImageStatus::NONE.to_string(),
                        None,
                        format!("stack JSON deserialize failed: {err}"),
                    ));
                }
            }
        };

        let (has_devcontainer, has_devcontainer_lock) = stack
            .as_ref()
            .map(|s| {
                (
                    s.manifest_signals.has_devcontainer,
                    s.manifest_signals.has_devcontainer_lock,
                )
            })
            .unwrap_or((false, false));

        // Generate a starter only when the user has nothing committed and
        // we have a non-trivial stack to base it on. A missing stack
        // falls back to a minimal Ubuntu-based starter so the banner is
        // still actionable on the first mirror fetch.
        let starter_json = if !has_devcontainer {
            Some(match stack.as_ref() {
                Some(s) => generate_starter(s),
                None => generate_starter(&Stack::empty()),
            })
        } else {
            None
        };

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

        // Only bother checking GitHub for an open setup PR when the user
        // would actually see the button (i.e. `has_devcontainer == false`).
        // Failures here are non-fatal — the banner still works, the button
        // just falls back to "Open PR" and retries on click.
        let open_setup_pr = if has_devcontainer {
            None
        } else {
            find_open_setup_pr(&repo, &input.project).await
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
            has_devcontainer,
            has_devcontainer_lock,
            image_tag: image.tag,
            image_status: image.status,
            image_last_error: image.last_error,
            starter_json,
            open_setup_pr,
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

    /// Open (or return the existing) PR that adds a starter
    /// `.devcontainer/devcontainer.json` to the project's default branch.
    ///
    /// The server:
    ///   1. Looks up an already-open PR on the stable
    ///      `djinn/setup-devcontainer` head branch and returns it verbatim
    ///      if found (idempotent on double-clicks).
    ///   2. Otherwise creates/updates that branch from the default branch
    ///      via GitHub's Git Refs + Contents APIs — no local worktree
    ///      needed.
    ///   3. Opens the PR against the default branch and returns the URL.
    ///
    /// All work is performed with a GitHub App installation token cached
    /// on the project row, so the commit is attributed to `djinn-bot[bot]`
    /// rather than the invoking user.
    #[tool(
        description = "Open a PR adding .devcontainer/devcontainer.json to the project's repo using the detected stack. Returns the URL of a new or already-open PR on the djinn/setup-devcontainer branch."
    )]
    pub async fn devcontainer_open_pr(
        &self,
        Parameters(input): Parameters<DevcontainerOpenPrParams>,
    ) -> Json<DevcontainerOpenPrResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // ── Resolve project coords + installation ────────────────────────
        let (owner, repo_name) = match repo.get_github_coords(&input.project).await {
            Ok(Some(coords)) => coords,
            Ok(None) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(
                        "project has no GitHub owner/repo — was it added via GitHub?".into(),
                    ),
                });
            }
            Err(err) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(format!("coord lookup failed: {err}")),
                });
            }
        };

        let installation_id = match repo.get_installation_id(&input.project).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(
                        "project has no cached GitHub App installation id — reinstall the Djinn App".into(),
                    ),
                });
            }
            Err(err) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(format!("installation lookup failed: {err}")),
                });
            }
        };

        let default_branch = match repo.get_default_branch(&input.project).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some("project row is missing a default branch".into()),
                });
            }
            Err(err) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(format!("default branch lookup failed: {err}")),
                });
            }
        };

        let client = GitHubApiClient::for_installation(installation_id);

        // ── Short-circuit when a PR is already open ──────────────────────
        let head_filter = format!("{owner}:{DEVCONTAINER_PR_BRANCH}");
        match client
            .list_pulls_by_head_with_state(&owner, &repo_name, &head_filter, "open")
            .await
        {
            Ok(prs) => {
                if let Some(pr) = prs.into_iter().next() {
                    return Json(DevcontainerOpenPrResponse {
                        pr: Some(DevcontainerPrRef {
                            url: pr.html_url,
                            number: pr.number,
                        }),
                        already_open: true,
                        error: None,
                    });
                }
            }
            Err(err) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(format!("failed to query existing PRs: {err}")),
                });
            }
        }

        // ── Build the starter JSON from the detected stack ───────────────
        let stack_raw = repo.get_stack(&input.project).await.unwrap_or_default();
        let stack: Option<Stack> = stack_raw
            .as_deref()
            .filter(|s| !s.trim().is_empty() && s.trim() != "{}")
            .and_then(|s| serde_json::from_str(s).ok());
        let starter = match stack.as_ref() {
            Some(s) => generate_starter(s),
            None => generate_starter(&Stack::empty()),
        };

        // ── Branch off the default branch (idempotent) ───────────────────
        let base_ref = format!("heads/{default_branch}");
        let base_sha = match client.get_ref(&owner, &repo_name, &base_ref).await {
            Ok(Some(sha)) => sha,
            Ok(None) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(format!(
                        "default branch {default_branch:?} not found on GitHub"
                    )),
                });
            }
            Err(err) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(format!("default branch lookup failed: {err}")),
                });
            }
        };

        let new_ref = format!("refs/heads/{DEVCONTAINER_PR_BRANCH}");
        if let Err(err) = client
            .create_ref(&owner, &repo_name, &new_ref, &base_sha)
            .await
        {
            return Json(DevcontainerOpenPrResponse {
                pr: None,
                already_open: false,
                error: Some(format!("failed to create setup branch: {err}")),
            });
        }

        // ── Write the starter file on the setup branch ───────────────────
        // If the branch already existed from a prior attempt and the file
        // is present, update by its blob SHA rather than creating.
        let prev_sha = client
            .get_file_sha(&owner, &repo_name, DEVCONTAINER_PATH, DEVCONTAINER_PR_BRANCH)
            .await
            .ok()
            .flatten();
        if let Err(err) = client
            .put_file(
                &owner,
                &repo_name,
                DEVCONTAINER_PATH,
                DEVCONTAINER_PR_BRANCH,
                "Add Djinn devcontainer config",
                starter.as_bytes(),
                prev_sha.as_deref(),
            )
            .await
        {
            return Json(DevcontainerOpenPrResponse {
                pr: None,
                already_open: false,
                error: Some(format!("failed to commit devcontainer.json: {err}")),
            });
        }

        // ── Open the PR ─────────────────────────────────────────────────
        let body = "Adds a starter `.devcontainer/devcontainer.json` generated by Djinn from \
             this repo's detected stack. Djinn needs this committed before it can \
             run tasks in a sandboxed devcontainer.\n\n\
             Review the features and base image and edit as needed before merging.\n\n\
             _Opened by djinn-bot via the Djinn UI._";
        let pr = match client
            .create_pull_request(
                &owner,
                &repo_name,
                CreatePrParams {
                    title: "Add Djinn devcontainer config".into(),
                    body: body.into(),
                    head: DEVCONTAINER_PR_BRANCH.into(),
                    base: default_branch,
                    maintainer_can_modify: Some(true),
                    draft: None,
                },
            )
            .await
        {
            Ok(pr) => pr,
            Err(err) => {
                return Json(DevcontainerOpenPrResponse {
                    pr: None,
                    already_open: false,
                    error: Some(format!("failed to open PR: {err}")),
                });
            }
        };

        Json(DevcontainerOpenPrResponse {
            pr: Some(DevcontainerPrRef {
                url: pr.html_url,
                number: pr.number,
            }),
            already_open: false,
            error: None,
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
