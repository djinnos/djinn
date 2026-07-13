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

/// Content seeded into `.djinn/.gitignore` when a project is added.
///
/// This covers the `.djinn/` directory tree (including worktree metadata
/// and any scratch files that land under `.djinn/`).  The `worktrees/`
/// entry prevents the server-managed worktree clones from being tracked;
/// the scratch/editor/cache entries are **defense-in-depth** — the
/// authoritative junk filter lives in `Workspace::commit` staging
/// (`djinn-workspace/src/commit_safety.rs`).
const DJINN_GITIGNORE: &str = "\
worktrees/
# ── Defense-in-depth: common worker scratch / editor / cache droppings ──
# These patterns mirror the authoritative commit-safety filter in
# djinn-workspace::commit_safety but only cover files that land under
# .djinn/.  They are a convenience layer so untracked scratch never
# even shows up in `git status` for this subtree.
patch.txt
patch*
test.txt
test2.txt
test3.txt
*.swp
*.swo
*~
*.bak
*.tmp
*.log
.DS_Store
Thumbs.db
.cache/
";

/// Content seeded into the project root `.gitignore` when a project is
/// added and no root `.gitignore` exists yet.
///
/// Covers common root-level worker scratch files and editor/cache
/// droppings.  This is **defense-in-depth only**: the authoritative
/// protection is the filtered `Workspace::commit` path
/// (`djinn-workspace/src/commit_safety`).  When a root `.gitignore`
/// already exists (the common case for cloned repos), this content is
/// **not** written — the commit filter remains the sole guard for
/// existing projects.
///
/// All patterns below are anchored with a leading `/` so they apply
/// **only at the repository root**.  This is deliberate: it keeps
/// intentional fixture / testdata files under `tests/fixtures/`,
/// `testdata/`, `fixtures/`, etc. available even when their basenames
/// resemble scratch artifacts (e.g. `tests/fixtures/test.txt`,
/// `testdata/patch_expected.txt`).  The authoritative commit filter
/// applies its own root-only / fixture-allowlist logic, so we
/// deliberately do NOT mirror broad unanchored patterns like `*.log`
/// or `*.tmp` here — those would hide fixture content.
const DJINN_ROOT_SCRATCH_GITIGNORE: &str = "\
# ── Djinn defense-in-depth: worker scratch / editor / cache ignores ──
# Auto-seeded by Djinn on project add.  These patterns are a
# convenience layer — the authoritative junk filter is the staged-commit
# path in djinn-workspace::commit_safety.  Safe to remove.
#
# Every pattern below is root-anchored (leading `/`) so it applies
# only at the repo root.  Intentional fixture / testdata files with
# scratch-like basenames (tests/fixtures/test.txt, testdata/patch_*)
# remain tracked.  The authoritative commit filter handles the rest.
# ── Root-level worker scratch droppings ──
/patch
/patch*
/patch.txt
/test.txt
/test2.txt
/test3.txt
# ── Root-level editor / cache droppings ──
/.DS_Store
/Thumbs.db
/.gitattributes.bak
/*.swp
/*.swo
/*~
/*.bak
/.cache/
";

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

        // 5b. Configure git user.name/user.email so commits created by the
        //     server/agents use the active App's bot login. Manifest-created
        //     Apps have unique slugs, so a hard-coded `djinn-bot[bot]` would
        //     misattribute them.
        let (bot_name, bot_email) = djinn_provider::github_app::bot_git_identity();
        for (key, value) in [
            ("user.name", bot_name.as_str()),
            ("user.email", bot_email.as_str()),
        ] {
            if let Err(e) = crate::tools::git_ops::git_config_set(&clone_path, key, value).await {
                tracing::warn!(
                    path = %clone_path, key, error = %e,
                    "project_add_from_github: failed to set git config"
                );
            }
        }

        // 6. Seed .djinn/ conveniences and defense-in-depth ignores.
        //
        // `.djinn/.gitignore` covers the .djinn/ subtree (worktree metadata,
        // internal scratch).  A root-level `.gitignore` is seeded only when
        // none exists yet — for repos that already have one the authoritative
        // commit-safety filter in Workspace::commit remains the sole guard.
        let djinn_dir = std::path::Path::new(&clone_path).join(".djinn");
        let _ = fs::create_dir_all(&djinn_dir).await;
        let gitignore_path = djinn_dir.join(".gitignore");
        if !gitignore_path.exists() {
            let _ = fs::write(&gitignore_path, DJINN_GITIGNORE).await;
        }

        // Defense-in-depth: seed root-level scratch ignores only if the
        // project has no `.gitignore` at all (common for fresh/empty repos;
        // cloned repos almost always have one already).
        let root_gitignore = std::path::Path::new(&clone_path).join(".gitignore");
        if !root_gitignore.exists() {
            let _ = fs::write(&root_gitignore, DJINN_ROOT_SCRATCH_GITIGNORE).await;
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

    // ── Defense-in-depth gitignore template tests ──────────────────────

    /// Verify `.djinn/.gitignore` content includes all expected scratch
    /// patterns.  These are defense-in-depth only; the authoritative
    /// filter is `Workspace::commit` staging
    /// (`djinn-workspace/src/commit_safety`).
    #[test]
    fn djinn_gitignore_includes_worktrees_ignore() {
        assert!(
            DJINN_GITIGNORE.contains("worktrees/"),
            "DJINN_GITIGNORE must contain worktrees/ to prevent tracking worktree clones"
        );
    }

    #[test]
    fn djinn_gitignore_includes_scratch_patterns() {
        for pattern in &["patch.txt", "test.txt", "test2.txt", "test3.txt"] {
            assert!(
                DJINN_GITIGNORE.contains(pattern),
                "DJINN_GITIGNORE missing scratch pattern: {pattern}"
            );
        }
        // patch* glob covers patch.txt, patch.json, patch_output, etc.
        assert!(
            DJINN_GITIGNORE.contains("patch*"),
            "DJINN_GITIGNORE must contain patch* glob pattern"
        );
    }

    #[test]
    fn djinn_gitignore_includes_editor_droppings() {
        for pattern in &["*.swp", "*.swo", "*~", "*.bak", ".DS_Store", "Thumbs.db"] {
            assert!(
                DJINN_GITIGNORE.contains(pattern),
                "DJINN_GITIGNORE missing editor/cache pattern: {pattern}"
            );
        }
    }

    #[test]
    fn djinn_gitignore_includes_temp_log_patterns() {
        for pattern in &["*.tmp", "*.log"] {
            assert!(
                DJINN_GITIGNORE.contains(pattern),
                "DJINN_GITIGNORE missing temp/log pattern: {pattern}"
            );
        }
    }

    #[test]
    fn djinn_gitignore_includes_cache_dir() {
        assert!(
            DJINN_GITIGNORE.contains(".cache/"),
            "DJINN_GITIGNORE must exclude .cache/ directory"
        );
    }

    /// Verify the root-level scratch gitignore includes expected patterns
    /// (all root-anchored so they don't leak into fixture/testdata dirs).
    #[test]
    fn root_scratch_gitignore_includes_anchored_scratch_names() {
        // Root-anchored scratch names (leading `/`).
        for pattern in &["/patch.txt", "/test.txt", "/test2.txt", "/test3.txt"] {
            assert!(
                DJINN_ROOT_SCRATCH_GITIGNORE.contains(pattern),
                "DJINN_ROOT_SCRATCH_GITIGNORE missing root-anchored scratch pattern: {pattern}"
            );
        }
        // Root-anchored patch prefix (covers /patch, /patch.json, /patch_output, …).
        assert!(
            DJINN_ROOT_SCRATCH_GITIGNORE.contains("/patch*"),
            "DJINN_ROOT_SCRATCH_GITIGNORE must contain root-anchored /patch* glob"
        );
        assert!(
            DJINN_ROOT_SCRATCH_GITIGNORE
                .lines()
                .any(|l| l.trim() == "/patch"),
            "DJINN_ROOT_SCRATCH_GITIGNORE must contain a root-anchored /patch entry \
             (covers a bare root-level 'patch' artifact)"
        );
    }

    #[test]
    fn root_scratch_gitignore_includes_anchored_editor_droppings() {
        for pattern in &[
            "/*.swp",
            "/*.swo",
            "/*~",
            "/*.bak",
            "/.DS_Store",
            "/Thumbs.db",
            "/.gitattributes.bak",
        ] {
            assert!(
                DJINN_ROOT_SCRATCH_GITIGNORE.contains(pattern),
                "DJINN_ROOT_SCRATCH_GITIGNORE missing root-anchored editor/cache pattern: {pattern}"
            );
        }
    }

    #[test]
    fn root_scratch_gitignore_includes_anchored_cache_dir() {
        assert!(
            DJINN_ROOT_SCRATCH_GITIGNORE.contains("/.cache/"),
            "DJINN_ROOT_SCRATCH_GITIGNORE must contain root-anchored /.cache/ entry"
        );
    }

    /// Root-scratch patterns must be root-anchored so they do NOT match
    /// intentional fixture/testdata basenames.  This is the regression
    /// guard for the review feedback about unanchored patterns like
    /// `test.txt`, `patch*`, `*.log`, `*.tmp` hiding fixture content.
    #[test]
    fn root_scratch_gitignore_does_not_contain_unanchored_scratch_names() {
        // Bare basename patterns (no leading `/`) would match at any depth
        // and hide files like `tests/fixtures/test.txt`,
        // `testdata/patch_expected.txt`, `fixtures/output.log`.
        for unanchored in &[
            "\npatch.txt\n",
            "\ntest.txt\n",
            "\ntest2.txt\n",
            "\ntest3.txt\n",
            "\npatch*\n",
            "\n.DS_Store\n",
            "\nThumbs.db\n",
            "\n.gitattributes.bak\n",
        ] {
            assert!(
                !DJINN_ROOT_SCRATCH_GITIGNORE.contains(unanchored),
                "DJINN_ROOT_SCRATCH_GITIGNORE contains unanchored pattern {unanchored:?} \
                 which would match fixture/testdata basenames at any depth; \
                 all root patterns must be prefixed with `/`"
            );
        }
    }

    /// Broad unanchored patterns like `*.log` and `*.tmp` are intentionally
    /// omitted from the root template because they would hide fixture
    /// content (logs and tmp files are sometimes used as test fixtures).
    /// The authoritative commit-safety filter handles those cases using
    /// its own root-only / fixture-allowlist logic.
    #[test]
    fn root_scratch_gitignore_omits_broad_temp_log_patterns() {
        for broad in &["*.tmp", "*.log"] {
            assert!(
                !DJINN_ROOT_SCRATCH_GITIGNORE.contains(broad),
                "DJINN_ROOT_SCRATCH_GITIGNORE must not contain broad pattern {broad:?}; \
                 the authoritative commit filter covers temp/log files and \
                 applying it here at any depth would hide fixture content"
            );
        }
    }

    /// Verify the fixture-preservation guard explicitly: every fixture
    /// basename that the reviewer flagged (tests/fixtures/test.txt,
    /// testdata/patch_expected.txt, fixtures/output.log,
    /// tests/fixtures/example.tmp) must not be ignored by the root
    /// template.  We can't introspect gitignore resolution without a
    /// git repo, so we assert the negative: the patterns that would
    /// cause those ignores are absent from the template.
    #[test]
    fn root_scratch_gitignore_preserves_named_fixture_paths() {
        // None of these bare patterns (which would catch fixture paths) are present.
        assert!(
            !DJINN_ROOT_SCRATCH_GITIGNORE
                .lines()
                .any(|l| l.trim() == "test.txt"),
            "Bare 'test.txt' pattern would ignore tests/fixtures/test.txt"
        );
        assert!(
            !DJINN_ROOT_SCRATCH_GITIGNORE
                .lines()
                .any(|l| l.trim() == "patch*"),
            "Bare 'patch*' pattern would ignore testdata/patch_expected.txt"
        );
        assert!(
            !DJINN_ROOT_SCRATCH_GITIGNORE
                .lines()
                .any(|l| l.trim() == "*.log"),
            "Bare '*.log' pattern would ignore fixtures/output.log"
        );
        assert!(
            !DJINN_ROOT_SCRATCH_GITIGNORE
                .lines()
                .any(|l| l.trim() == "*.tmp"),
            "Bare '*.tmp' pattern would ignore tests/fixtures/example.tmp"
        );
    }

    /// Both templates document their defense-in-depth nature.
    #[test]
    fn gitignore_templates_document_defense_in_depth() {
        assert!(
            DJINN_GITIGNORE.contains("defense-in-depth")
                || DJINN_GITIGNORE.contains("Defense-in-depth"),
            "DJINN_GITIGNORE should document its defense-in-depth nature"
        );
        assert!(
            DJINN_ROOT_SCRATCH_GITIGNORE.contains("defense-in-depth")
                || DJINN_ROOT_SCRATCH_GITIGNORE.contains("Defense-in-depth"),
            "DJINN_ROOT_SCRATCH_GITIGNORE should document its defense-in-depth nature"
        );
    }

    /// Both templates reference the authoritative commit-safety filter.
    #[test]
    fn gitignore_templates_reference_authoritative_filter() {
        assert!(
            DJINN_GITIGNORE.contains("commit_safety")
                || DJINN_GITIGNORE.contains("Workspace::commit"),
            "DJINN_GITIGNORE should reference the authoritative commit filter"
        );
        assert!(
            DJINN_ROOT_SCRATCH_GITIGNORE.contains("commit_safety")
                || DJINN_ROOT_SCRATCH_GITIGNORE.contains("Workspace::commit"),
            "DJINN_ROOT_SCRATCH_GITIGNORE should reference the authoritative commit filter"
        );
    }

    /// Verify the templates do NOT contain fixture/testdata exclusions —
    /// fixture directories must remain allowed (intentional test data).
    /// The check skips comment lines (starting with `#`) and blank lines
    /// so explanatory comments referring to `fixtures/`/`testdata/` are
    /// permitted.
    #[test]
    fn gitignore_templates_preserve_fixture_directories() {
        for template in &[DJINN_GITIGNORE, DJINN_ROOT_SCRATCH_GITIGNORE] {
            let offending: Vec<&str> = template
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter(|l| l.contains("fixtures/") || l.contains("testdata/"))
                .collect();
            assert!(
                offending.is_empty(),
                "Template must not ignore fixture/testdata directories, \
                 offending pattern lines: {offending:?}"
            );
        }
    }

    /// End-to-end regression test for the reviewer feedback:
    /// create a real git repo, write the root gitignore template, and
    /// verify with `git check-ignore` that:
    ///
    /// 1. root-level scratch files (e.g. `/patch.txt`) ARE ignored, and
    /// 2. fixture paths with scratch-like basenames (e.g.
    ///    `tests/fixtures/test.txt`, `testdata/patch_expected.txt`) are
    ///    NOT ignored.
    #[test]
    fn root_scratch_gitignore_does_not_ignore_fixture_paths() {
        use std::process::Command;

        let tmp = std::env::temp_dir().join(format!(
            "djinn_root_gitignore_test_{}_{}",
            std::process::id(),
            unique_test_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");

        // Initialize a real git repo and write the root gitignore.
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&tmp)
                .output()
                .expect("git command failed to spawn");
            (
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        };

        let (code, _, err) = run(&["init", "-q", "."]);
        assert_eq!(code, 0, "git init failed: {err}");

        std::fs::write(tmp.join(".gitignore"), DJINN_ROOT_SCRATCH_GITIGNORE)
            .expect("write .gitignore");

        // Configure a committer so git status / check-ignore don't complain.
        let _ = run(&["config", "user.email", "test@example.com"]);
        let _ = run(&["config", "user.name", "test"]);

        // The cases the reviewer flagged as broken under the old template.
        // Each (path, expected_ignored) pair.
        let cases: &[(&str, bool)] = &[
            // Root scratch files must be ignored.
            ("patch.txt", true),
            ("test.txt", true),
            ("test2.txt", true),
            ("test3.txt", true),
            ("patch", true),
            ("patch.json", true),
            ("patch_output", true),
            (".DS_Store", true),
            ("Thumbs.db", true),
            (".gitattributes.bak", true),
            ("scratch.swp", true),
            ("scratch.swo", true),
            ("scratch~", true),
            ("scratch.bak", true),
            // Fixture / testdata paths with scratch-like basenames MUST NOT be ignored.
            ("tests/fixtures/test.txt", false),
            ("tests/fixtures/test2.txt", false),
            ("tests/fixtures/patch.txt", false),
            ("tests/fixtures/example.tmp", false),
            ("testdata/patch_expected.txt", false),
            ("fixtures/output.log", false),
            ("testdata/sample.log", false),
            // Deeply nested fixture paths must also remain tracked.
            ("crates/x/tests/fixtures/data.json", false),
            // Intentionally-tracked source files must not be ignored.
            ("src/main.rs", false),
            ("README.md", false),
        ];

        for (path, expected_ignored) in cases {
            let (code, stdout, stderr) = run(&["check-ignore", "-v", "--no-index", path]);
            let actually_ignored = code == 0;
            assert_eq!(
                actually_ignored, *expected_ignored,
                "path {path}: expected_ignored={}, got={}\n\
                 stdout: {stdout}\nstderr: {stderr}\n\
                 gitignore content:\n{DJINN_ROOT_SCRATCH_GITIGNORE}",
                *expected_ignored, actually_ignored,
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Test-local nanos counter for unique temp dir names.
    /// Uses `time::OffsetDateTime` (the project's preferred clock) and
    /// its timestamp nanos so concurrent test runs don't collide.
    fn unique_test_nanos() -> String {
        let now = time::OffsetDateTime::now_utc();
        format!("{}", now.unix_timestamp_nanos())
    }
}
