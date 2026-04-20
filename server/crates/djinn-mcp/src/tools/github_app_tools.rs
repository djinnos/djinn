//! MCP tools for the GitHub **App** (installations + install URL).
//!
//! These tools back the desktop UI for "Connect GitHub" and the repository
//! picker in the Add-Project flow. They do NOT mint or expose installation
//! tokens — those are server-internal and used by clone/fetch/push paths in
//! [`crate::tools::project_tools`].

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use djinn_db::OrgConfigRepository;
use djinn_provider::github_app::{Installation, get_installation_by_id, install_url};

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
    pub id: i64,
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
        id: i.id as i64,
        account_login: i.account_login,
        account_type: i.account_type,
        install_url,
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

#[tool_router(router = github_app_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Return the single installation bound to this deployment via
    /// `org_config`.
    ///
    /// Under the "one deployment = one GitHub org" model, a deployment is
    /// pinned to exactly one installation id at setup time. We read that
    /// binding from `org_config` and fetch the installation metadata with
    /// `GET /app/installations/{id}` authenticated by an App JWT. If the
    /// deployment hasn't been bound yet, we surface an explicit error and
    /// the install URL so the UI can route the user through the manifest
    /// flow.
    #[tool(
        description = "Return the single GitHub App installation bound to this Djinn deployment (from org_config). Uses the App's private-key JWT — no user session needed. Returns a one-element list with the installation's id, account, type, and a URL to manage it. If the deployment is not yet bound to an organization, returns an empty list plus `install_url` pointing at the App's installation page."
    )]
    pub async fn github_app_installations(
        &self,
        Parameters(_): Parameters<GithubAppInstallationsParams>,
    ) -> Json<GithubAppInstallationsResponse> {
        // If the App itself isn't provisioned yet (no GITHUB_APP_ID), say so
        // cleanly instead of bubbling a cryptic JWT-mint error. The UI can
        // route the user to the manifest flow on this signal.
        if std::env::var("GITHUB_APP_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_none()
        {
            return Json(GithubAppInstallationsResponse {
                status: "error: GitHub App not configured".into(),
                installations: vec![],
                install_url: install_url(),
            });
        }

        // Source of truth: the singleton `org_config` row written by the
        // in-UI installation picker.
        let installation_id = {
            let org_repo = OrgConfigRepository::new(self.state.db().clone());
            match org_repo.get().await {
                Ok(Some(cfg)) => cfg.installation_id as u64,
                Ok(None) => {
                    return Json(GithubAppInstallationsResponse {
                        status: "error: deployment not bound to an organization".into(),
                        installations: vec![],
                        install_url: install_url(),
                    });
                }
                Err(e) => {
                    return Json(GithubAppInstallationsResponse {
                        status: format!("error: read org_config: {e}"),
                        installations: vec![],
                        install_url: install_url(),
                    });
                }
            }
        };

        match get_installation_by_id(installation_id).await {
            Ok(install) => Json(GithubAppInstallationsResponse {
                status: "ok".into(),
                installations: vec![installation_to_entry(install)],
                install_url: install_url(),
            }),
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
