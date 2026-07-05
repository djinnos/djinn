use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::server::DjinnMcpServer;
use djinn_core::auth_context::current_user_token;
use djinn_core::models::Project;
use djinn_core::paths::project_dir;
use djinn_db::ProjectRepository;

/// Resolve a project handle (UUID or `owner/repo` slug) to a
/// fully-loaded `Project`. Single source of truth for the many
/// MCP tools that take a `project` parameter.
pub(super) async fn resolve_project(
    repo: &ProjectRepository,
    project_ref: &str,
) -> djinn_db::Result<Option<Project>> {
    let Some(id) = repo.resolve(project_ref).await? else {
        return Ok(None);
    };
    repo.get(&id).await
}

/// Run `git fetch --all --prune` inside `path`. Best-effort refresh for an
/// existing server-managed clone.
async fn git_fetch_in(path: &str) -> Result<(), String> {
    crate::tools::git_ops::git_fetch_in(path).await
}

const DJINN_GITIGNORE: &str = "worktrees/\n";

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProjectRemoveParams {
    /// Project identifier — either the UUID (`project_id`) or the
    /// canonical `"owner/repo"` slug. Resolved via
    /// `ProjectRepository::resolve`.
    pub project: String,
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
pub struct ProjectBranchesParams {
    /// Project UUID to resolve the server-owned clone path for.
    pub project_id: String,
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
    pub github_owner: String,
    pub github_repo: String,
}

impl ProjectInfo {
    pub(crate) fn from_project(p: &Project) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            github_owner: p.github_owner.clone(),
            github_repo: p.github_repo.clone(),
        }
    }

    pub(crate) fn unknown(name: String) -> Self {
        Self {
            id: String::new(),
            name,
            github_owner: String::new(),
            github_repo: String::new(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectBranchesResponse {
    pub status: String,
    pub branches: Vec<String>,
    pub current: Option<String>,
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

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = lifecycle_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Unregister a project from Djinn.
    #[tool(
        description = "Remove a project from the Djinn registry. Accepts the project UUID or the canonical \"owner/repo\" slug."
    )]
    pub async fn project_remove(
        &self,
        Parameters(input): Parameters<ProjectRemoveParams>,
    ) -> Json<ProjectRemoveResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        let id = match repo.resolve(&input.project).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Json(ProjectRemoveResponse {
                    status: format!("error: project not found: {}", input.project),
                    project: ProjectInfo::unknown(input.project),
                });
            }
            Err(e) => {
                return Json(ProjectRemoveResponse {
                    status: format!("error: {e}"),
                    project: ProjectInfo::unknown(input.project),
                });
            }
        };

        let project = match repo.get(&id).await {
            Ok(Some(p)) => p,
            _ => {
                return Json(ProjectRemoveResponse {
                    status: format!("error: project not found: {}", input.project),
                    project: ProjectInfo::unknown(input.project),
                });
            }
        };
        let info = ProjectInfo::from_project(&project);

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
                projects: projects.iter().map(ProjectInfo::from_project).collect(),
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
                project: ProjectInfo::unknown(input.name.unwrap_or_default()),
            });
        }
        let display_name = input.name.unwrap_or_else(|| repo.clone());

        // 1. Must have a session user token from the task-local (set by the
        //    HTTP MCP handler after resolving the `djinn_session` cookie).
        let Some(user_access_token) = current_user_token() else {
            return Json(ProjectAddResponse {
                status: "error: sign in with GitHub required".into(),
                project: ProjectInfo::unknown(display_name),
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
                        project: ProjectInfo::unknown(display_name),
                    });
                }
            }
        };

        let default_branch = input.git_ref.clone().unwrap_or_else(|| "main".to_string());

        // 3. Synthesize the server-side clone path from the owner/repo
        //    coords. The path is never persisted — every consumer calls
        //    `djinn_core::paths::project_dir` with the project's coords.
        let clone_path_buf = project_dir(&owner, &repo);
        let clone_path = clone_path_buf.to_string_lossy().into_owned();

        // Idempotent: if already registered, fast-path to `git fetch`.
        if let Ok(Some(existing)) = repo_db.get_by_github(&owner, &repo).await {
            let _ = fs::create_dir_all(&clone_path).await;
            if let Err(e) = git_fetch_in(&clone_path).await {
                tracing::warn!(
                    owner = %owner, repo = %repo, error = %e,
                    "project_add_from_github: fetch refresh failed",
                );
            }
            return Json(ProjectAddResponse {
                status: "ok".into(),
                project: ProjectInfo::from_project(&existing),
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
                    project: ProjectInfo::unknown(display_name),
                });
            }
        };
        let remote_url = format!(
            "https://x-access-token:{}@github.com/{owner}/{repo}.git",
            install_token.token
        );

        if !std::path::Path::new(&clone_path).join(".git").exists() {
            if let Err(e) =
                crate::tools::git_ops::git_clone_blob_none(&remote_url, &clone_path).await
            {
                return Json(ProjectAddResponse {
                    status: format!("error: {e}"),
                    project: ProjectInfo::unknown(display_name),
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
                if let Err(e) = crate::tools::git_ops::git_config_set(&clone_path, key, value).await
                {
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
                Some(installation_id),
            )
            .await
        {
            Ok(project) => {
                // Kick the mirror→stack→image→graph pipeline immediately so
                // the freshly added repo's stack gets detected and its image
                // enqueued now, instead of waiting up to a full mirror-fetch
                // tick. Fire-and-forget; the periodic tick is the backstop.
                self.state.trigger_mirror_refresh(&project.id).await;
                Json(ProjectAddResponse {
                    status: "ok".into(),
                    project: ProjectInfo::from_project(&project),
                })
            }
            Err(e) => Json(ProjectAddResponse {
                status: format!("error: {e}"),
                project: ProjectInfo::unknown(display_name),
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

        // Synthesize the clone path from the project's GitHub coords —
        // every consumer derives this locally, none of it is persisted.
        let path_buf = project_dir(&project.github_owner, &project.github_repo);
        let path = path_buf.to_string_lossy().into_owned();
        if !Path::new(&path).join(".git").exists() {
            return Json(ProjectBranchesResponse {
                status: format!("error: not a git repository: {path}"),
                branches: vec![],
                current: None,
            });
        }

        let current = match crate::tools::git_ops::git_current_branch(&path).await {
            Ok(branch) => branch,
            Err(e) => {
                return Json(ProjectBranchesResponse {
                    status: format!("error: git rev-parse failed: {e}"),
                    branches: vec![],
                    current: None,
                });
            }
        };

        let list_stdout = match crate::tools::git_ops::git_local_branches(&path).await {
            Ok(out) => out,
            Err(e) => {
                return Json(ProjectBranchesResponse {
                    status: format!("error: git branch failed: {e}"),
                    branches: vec![],
                    current,
                });
            }
        };
        let parsed = parse_branch_list(&list_stdout);
        let branches = order_branches(parsed, current.as_deref());

        Json(ProjectBranchesResponse {
            status: "ok".into(),
            branches,
            current,
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
