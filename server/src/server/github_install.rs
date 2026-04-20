//! Installation-picker HTTP routes (`/api/github/installations[/select]`).
//!
//! Provides the in-UI flow that replaces the operator-edits-the-Secret step:
//! the browser asks the server which GitHub App installations exist, the
//! operator clicks one, and the server writes the deployment-to-org binding
//! into `org_config`.
//!
//! Both routes are public-but-gated:
//!   * The App credentials (`app_config`) **must** be configured — both
//!     return `503 SERVICE_UNAVAILABLE` otherwise. Without an App JWT we
//!     can't talk to GitHub at all.
//!   * No session is required. The picker is the prerequisite to sign-in,
//!     and on a fresh deployment no user can have signed in yet.
//!
//! The `OrgBinding` env-loaded path (`GITHUB_INSTALLATION_ID` + friends)
//! remains the override for fully-automated CI deploys; when env binding is
//! present the UI never reaches the picker (`needs_app_install: false`).
//!
//! See `server/src/server/auth.rs::setup_status` for the gating signals
//! the UI consumes to decide which screen to render.

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use djinn_db::{NewOrgConfig, OrgConfigRepository};
use djinn_provider::github_app::jwt::mint_app_jwt_anyhow;

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "djinn-server";

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/github/installations", get(list_installations))
        .route(
            "/api/github/installations/select",
            post(select_installation),
        )
}

// ─── Wire types ───────────────────────────────────────────────────────────────

/// Picker row: one App installation as the UI needs to render it.
///
/// Mirrors GitHub's `GET /app/installations` shape closely so the UI can
/// distinguish "all repos" from "selected repos" and link directly to the
/// installation settings page.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InstallationSummary {
    pub installation_id: u64,
    pub account_login: String,
    pub account_id: u64,
    /// "User" or "Organization".
    pub account_type: String,
    /// "all" or "selected".
    pub repository_selection: String,
    pub html_url: String,
}

#[derive(Deserialize)]
struct SelectRequest {
    installation_id: u64,
}

#[derive(Serialize)]
struct SelectResponse {
    installation_id: u64,
    account_login: String,
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /api/github/installations` — proxy `GET /app/installations` to GitHub
/// using the App JWT, return a UI-friendly list.
async fn list_installations(State(state): State<AppState>) -> Response {
    if state.app_config().await.is_none() {
        return app_unconfigured_response();
    }

    match fetch_app_installations().await {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "GET /api/github/installations failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch installations from GitHub: {e}"),
            )
                .into_response()
        }
    }
}

/// `POST /api/github/installations/select` — validate the chosen id is in
/// `GET /app/installations` and write the `org_config` row.
async fn select_installation(
    State(state): State<AppState>,
    Json(req): Json<SelectRequest>,
) -> Response {
    let cfg = match state.app_config().await {
        Some(c) => c,
        None => return app_unconfigured_response(),
    };

    if req.installation_id == 0 {
        return (StatusCode::BAD_REQUEST, "installation_id must be > 0").into_response();
    }

    let installations = match fetch_app_installations().await {
        Ok(list) => list,
        Err(e) => {
            tracing::error!(error = %e, "POST /api/github/installations/select: fetch failed");
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch installations from GitHub: {e}"),
            )
                .into_response();
        }
    };

    let Some(chosen) = installations
        .into_iter()
        .find(|i| i.installation_id == req.installation_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            format!(
                "installation_id {} is not visible to this GitHub App; \
                 reload the picker and try again",
                req.installation_id
            ),
        )
            .into_response();
    };

    let repo = OrgConfigRepository::new(state.db().clone());
    let result = repo
        .set_or_replace(NewOrgConfig {
            github_org_id: chosen.account_id as i64,
            github_org_login: &chosen.account_login,
            app_id: cfg.app_id as i64,
            installation_id: chosen.installation_id as i64,
        })
        .await;

    match result {
        Ok(_) => {
            tracing::info!(
                installation_id = chosen.installation_id,
                account = %chosen.account_login,
                "installation picker: bound org_config",
            );
            Json(SelectResponse {
                installation_id: chosen.installation_id,
                account_login: chosen.account_login,
            })
            .into_response()
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                installation_id = chosen.installation_id,
                "installation picker: set_or_replace failed",
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to persist org binding",
            )
                .into_response()
        }
    }
}

// ─── GitHub API helper ────────────────────────────────────────────────────────

/// Fetch `GET /app/installations` and project to the UI shape.
///
/// We re-implement the call rather than reusing
/// `djinn_provider::github_app::installations::list_installations_for_app`
/// because the picker needs `repository_selection` and `html_url`, which the
/// provider crate's `Installation` struct intentionally omits to keep the
/// public surface small. Keeping the picker-specific shape in this module
/// avoids leaking UI-only fields into the lower layers.
async fn fetch_app_installations() -> Result<Vec<InstallationSummary>, String> {
    #[derive(Deserialize)]
    struct RawInstallation {
        id: u64,
        account: Option<RawAccount>,
        #[serde(default)]
        repository_selection: Option<String>,
        #[serde(default)]
        html_url: Option<String>,
    }
    #[derive(Deserialize)]
    struct RawAccount {
        id: u64,
        #[serde(default)]
        login: Option<String>,
        #[serde(rename = "type", default)]
        account_type: Option<String>,
    }

    let jwt = mint_app_jwt_anyhow().map_err(|e| e.to_string())?;
    let client = Client::new();
    let resp = client
        .get(format!("{GITHUB_API}/app/installations"))
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("{status}: {body}"));
    }

    let raws: Vec<RawInstallation> = resp.json().await.map_err(|e| e.to_string())?;
    Ok(raws
        .into_iter()
        .map(|raw| {
            let (account_id, account_login, account_type) = match raw.account {
                Some(a) => (
                    a.id,
                    a.login.unwrap_or_default(),
                    a.account_type.unwrap_or_else(|| "User".into()),
                ),
                None => (0, String::new(), "User".into()),
            };
            InstallationSummary {
                installation_id: raw.id,
                account_login,
                account_id,
                account_type,
                repository_selection: raw.repository_selection.unwrap_or_else(|| "all".into()),
                html_url: raw.html_url.unwrap_or_default(),
            }
        })
        .collect())
}

fn app_unconfigured_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "GitHub App credentials are not configured. Mount the \
         djinn-github-app Kubernetes Secret (see \
         server/docker/README.md) and restart the Pod.",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_installations_returns_503_when_app_unconfigured() {
        let state = test_helpers::test_app_state_in_memory().await;
        let resp = list_installations(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_installation_returns_503_when_app_unconfigured() {
        let state = test_helpers::test_app_state_in_memory().await;
        let resp = select_installation(
            State(state),
            Json(SelectRequest {
                installation_id: 42,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_installation_rejects_zero_id() {
        use std::sync::Arc;
        let state = test_helpers::test_app_state_in_memory().await;
        let cfg = djinn_provider::github_app::AppConfig {
            app_id: 1,
            slug: "djinn".into(),
            client_id: "Iv1.x".into(),
            client_secret: "y".into(),
            pem: "PEM".into(),
            webhook_secret: "w".into(),
            public_url: "http://127.0.0.1:8372".into(),
        };
        state.set_app_config(Some(Arc::new(cfg))).await;

        let resp = select_installation(
            State(state),
            Json(SelectRequest { installation_id: 0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
