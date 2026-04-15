//! MCP tools for the GitHub **App** (installations + install URL).
//!
//! These tools back the desktop UI for "Connect GitHub" and the repository
//! picker in the Add-Project flow. They do NOT mint or expose installation
//! tokens — those are server-internal and used by clone/fetch/push paths in
//! [`crate::tools::project_tools`].

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use djinn_core::auth_context::current_user_token;
use djinn_provider::github_app::{Installation, install_url, list_installations_for_user};

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
        // We need a user token to call GET /user/installations. The token
        // is threaded in via the `SESSION_USER_TOKEN` task-local, set by
        // the HTTP MCP handler after resolving the `djinn_session` cookie.
        let Some(access_token) = current_user_token() else {
            return Json(GithubAppInstallationsResponse {
                status: "error: sign in with GitHub required".into(),
                installations: vec![],
                install_url: install_url(),
            });
        };

        match list_installations_for_user(&access_token).await {
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
