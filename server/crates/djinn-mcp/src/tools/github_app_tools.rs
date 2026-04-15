//! MCP tools for the GitHub **App** (installations + install URL).
//!
//! These tools back the desktop UI for "Connect GitHub" and the repository
//! picker in the Add-Project flow. They do NOT mint or expose installation
//! tokens — those are server-internal and used by clone/fetch/push paths in
//! [`crate::tools::project_tools`].

use std::sync::Arc;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use djinn_provider::github_app::{
    Installation, install_url, list_installations_for_user,
};
use djinn_provider::oauth::github_app::GitHubAppTokens;
use djinn_provider::repos::CredentialRepository;

use crate::server::DjinnMcpServer;

// ── Params ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct GithubAppInstallationsParams {}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct GithubAppInstallUrlParams {}

// ── Responses ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InstallationEntry {
    /// Numeric installation ID (used for scoping API calls server-side).
    pub id: u64,
    /// `login` of the account (user or org) the App is installed on.
    pub account_login: String,
    /// Either "User" or "Organization".
    pub account_type: String,
    /// Direct URL to review/configure this installation.
    pub install_url: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GithubAppInstallationsResponse {
    pub status: String,
    pub installations: Vec<InstallationEntry>,
    /// URL to install the App when the user has no installations (or
    /// wants to add one). Always populated if `GITHUB_APP_SLUG` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GithubAppInstallUrlResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

fn installation_to_entry(i: Installation) -> InstallationEntry {
    let install_url = if i.account_type.eq_ignore_ascii_case("Organization") {
        format!(
            "https://github.com/organizations/{}/settings/installations",
            i.account_login
        )
    } else {
        format!("https://github.com/settings/installations/{}", i.id)
    };
    InstallationEntry {
        id: i.id,
        account_login: i.account_login,
        account_type: i.account_type,
        install_url,
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

#[tool_router(router = github_app_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// List GitHub App installations visible to the authenticated user.
    ///
    /// If the user has no installations, the response's top-level
    /// `install_url` points at the App's installation page so the UI can
    /// render an actionable "Install Djinn" button.
    #[tool(
        description = "List GitHub App installations the authenticated user can access. Returns each installation's id, account, type, and a URL to manage it. If no installations are found, include `install_url` pointing at the App's installation page."
    )]
    pub async fn github_app_installations(
        &self,
        Parameters(_): Parameters<GithubAppInstallationsParams>,
    ) -> Json<GithubAppInstallationsResponse> {
        let cred_repo = Arc::new(CredentialRepository::new(
            self.state.db().clone(),
            self.state.event_bus(),
        ));

        // We need a user token to call GET /user/installations. The
        // device-code-flow user token (legacy, stored at
        // __OAUTH_GITHUB_APP) is fine here — any user-to-server token works.
        let Some(tokens) = GitHubAppTokens::load_from_db(&cred_repo).await else {
            return Json(GithubAppInstallationsResponse {
                status: "error: Connect GitHub first".into(),
                installations: vec![],
                install_url: install_url(),
            });
        };

        match list_installations_for_user(&tokens.access_token).await {
            Ok(list) => {
                let installations: Vec<InstallationEntry> =
                    list.into_iter().map(installation_to_entry).collect();
                Json(GithubAppInstallationsResponse {
                    status: "ok".into(),
                    installations,
                    install_url: install_url(),
                })
            }
            Err(e) => Json(GithubAppInstallationsResponse {
                status: format!("error: {e}"),
                installations: vec![],
                install_url: install_url(),
            }),
        }
    }

    /// Return the "install this app" URL. Useful for UI buttons that
    /// don't need the installations list.
    #[tool(
        description = "Return the URL where a user can install the Djinn GitHub App. Reads GITHUB_APP_SLUG from the server env. Returns an error status if the App is not yet configured."
    )]
    pub async fn github_app_install_url(
        &self,
        Parameters(_): Parameters<GithubAppInstallUrlParams>,
    ) -> Json<GithubAppInstallUrlResponse> {
        match install_url() {
            Some(url) => Json(GithubAppInstallUrlResponse {
                status: "ok".into(),
                url: Some(url),
            }),
            None => Json(GithubAppInstallUrlResponse {
                status: "error: GITHUB_APP_SLUG not configured".into(),
                url: None,
            }),
        }
    }
}
