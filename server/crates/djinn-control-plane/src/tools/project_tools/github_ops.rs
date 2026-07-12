use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use djinn_db::OrgConfigRepository;

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GithubListReposParams {
    /// Max number of repositories to return (1..=100). Defaults to 30.
    #[serde(default)]
    pub per_page: Option<i64>,
}

// ── Response structs ─────────────────────────────────────────────────────────

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

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = project_github_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
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

        if djinn_provider::github_app::app_id().is_err() {
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
}
