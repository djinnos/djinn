//! Installation-picker HTTP routes (`/api/github/installations[/select]`).
//!
//! Provides the in-UI flow that replaces the operator-edits-the-Secret step:
//! the browser asks the server which GitHub App installations exist, the
//! operator clicks one, and the server writes the deployment-to-org binding
//! into `org_config`.
//!
//! Both routes are bootstrap-only and capability-gated:
//!   * The App credentials (`app_config`) **must** be configured — both
//!     return `503 SERVICE_UNAVAILABLE` otherwise. Without an App JWT we
//!     can't talk to GitHub at all.
//!   * No Djinn session is required. The picker is the prerequisite to
//!     sign-in, and on a fresh deployment no user can have signed in yet.
//!   * A short-lived HttpOnly capability cookie, issued only after the
//!     correlated GitHub install/OAuth continuation becomes ambiguous, is
//!     required. This prevents an arbitrary network caller from winning the
//!     first-binding race while preserving the no-session bootstrap flow.
//!
//! The picker is the only writer of the deployment-to-org binding. There
//! is no env override; the operator's only responsibility is the App
//! credentials Secret.
//!
//! See `server/src/server/auth.rs::setup_status` for the gating signals
//! the UI consumes to decide which screen to render.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use djinn_provider::github_app::jwt::mint_app_jwt_anyhow;
use djinn_provider::github_server::GitHubServerClient;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use crate::server::auth::{
    allow_user_installations, extract_cookie, installation_account_type_allowed, public_url,
};
use djinn_db::{NewOrgConfig, OrgConfigRepository};

/// Bearer capability issued after an install-triggered OAuth callback cannot
/// choose one installation unambiguously. It is deliberately path-scoped to
/// the two picker endpoints and is never a Djinn login session.
pub(super) const INSTALL_PICKER_CAPABILITY_COOKIE: &str = "djinn_install_picker";
const INSTALL_PICKER_CAPABILITY_PATH: &str = "/api/github/installations";
const INSTALL_PICKER_CAPABILITY_TTL_SECS: i64 = 60 * 10;

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
async fn list_installations(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if state.app_config().await.is_none() {
        return app_unconfigured_response();
    }

    if let Err(response) = require_unbound_picker(&state).await {
        return response;
    }
    if let Err(response) = require_picker_capability(&state, &headers).await {
        return response;
    }

    match fetch_app_installations().await {
        Ok(list) => {
            Json(selectable_installations(list, allow_user_installations())).into_response()
        }
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
    headers: HeaderMap,
    Json(req): Json<SelectRequest>,
) -> Response {
    let cfg = match state.app_config().await {
        Some(c) => c,
        None => return app_unconfigured_response(),
    };

    // The public picker is a bootstrap-only surface. Fail before trusting the
    // requested id or making an outbound GitHub request once setup is done.
    if let Err(response) = require_unbound_picker(&state).await {
        return response;
    }
    let capability = match require_picker_capability(&state, &headers).await {
        Ok(capability) => capability,
        Err(response) => return response,
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

    if !installation_account_type_allowed(&chosen.account_type, allow_user_installations()) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "GitHub account type '{}' cannot be selected. Organization installations are \
                 supported by default; User installations require \
                 DJINN_ALLOW_USER_INSTALLATIONS=true.",
                chosen.account_type
            ),
        )
            .into_response();
    }

    let repo = OrgConfigRepository::new(state.db().clone());
    let result = repo
        .create_if_absent(NewOrgConfig {
            github_org_id: chosen.account_id as i64,
            github_org_login: &chosen.account_login,
            app_id: cfg.app_id as i64,
            installation_id: chosen.installation_id as i64,
        })
        .await;

    match result {
        Ok(Some(_)) => {
            // The row insert is the durable one-shot boundary. Consume the
            // matching in-memory capability after it succeeds so transient
            // GitHub/DB failures remain retryable. The atomic INSERT prevents
            // two concurrent valid requests from replacing each other.
            let consumed = state
                .consume_pending_install_continuation(&capability)
                .await;
            if !consumed {
                tracing::warn!(
                    installation_id = chosen.installation_id,
                    "installation picker: binding persisted after capability was concurrently consumed",
                );
            }
            tracing::info!(
                installation_id = chosen.installation_id,
                account = %chosen.account_login,
                "installation picker: bound org_config",
            );
            let mut response_headers = HeaderMap::new();
            clear_picker_capability_cookie(&mut response_headers);
            (
                response_headers,
                Json(SelectResponse {
                    installation_id: chosen.installation_id,
                    account_login: chosen.account_login,
                }),
            )
                .into_response()
        }
        Ok(None) => {
            // A concurrent setup request won the singleton insert. Setup is
            // terminal either way; retire this bearer capability rather than
            // leaving a live bootstrap credential in the browser.
            let _ = state
                .consume_pending_install_continuation(&capability)
                .await;
            tracing::warn!(
                installation_id = chosen.installation_id,
                account = %chosen.account_login,
                "installation picker: refusing to replace existing org_config",
            );
            let mut response = already_bound_response();
            clear_picker_capability_cookie(response.headers_mut());
            response
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                installation_id = chosen.installation_id,
                "installation picker: set failed",
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
/// Uses `djinn_provider::github_server::GitHubServerClient` so the server
/// module does not directly construct an outbound HTTP client.
async fn fetch_app_installations() -> Result<Vec<InstallationSummary>, String> {
    #[cfg(test)]
    if let Some(result) = APP_INSTALLATIONS_RESULT_OVERRIDE.lock().unwrap().clone() {
        return result;
    }
    let jwt = mint_app_jwt_anyhow().map_err(|e| e.to_string())?;
    let raws = GitHubServerClient::new()
        .fetch_app_installations(&jwt)
        .await?;
    Ok(raws
        .into_iter()
        .map(|raw| {
            let account = raw.account();
            InstallationSummary {
                installation_id: raw.id,
                account_login: account.login,
                account_id: account.id,
                account_type: account.account_type,
                repository_selection: raw.repository_selection().to_string(),
                html_url: raw.html_url().to_string(),
            }
        })
        .collect())
}

#[cfg(test)]
static APP_INSTALLATIONS_RESULT_OVERRIDE: std::sync::Mutex<
    Option<Result<Vec<InstallationSummary>, String>>,
> = std::sync::Mutex::new(None);

fn selectable_installations(
    installations: Vec<InstallationSummary>,
    allow_users: bool,
) -> Vec<InstallationSummary> {
    installations
        .into_iter()
        .filter(|installation| {
            installation_account_type_allowed(&installation.account_type, allow_users)
        })
        .collect()
}

/// Move the manifest install continuation into a cookie scoped only to the
/// picker endpoints. The nonce remains server-side in `AppState`, so copying
/// or inventing an arbitrary cookie value cannot authorize a request.
pub(super) fn set_picker_capability_cookie(headers: &mut HeaderMap, value: &str) {
    let secure = if picker_cookie_secure() {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
        name = INSTALL_PICKER_CAPABILITY_COOKIE,
        path = INSTALL_PICKER_CAPABILITY_PATH,
        max_age = INSTALL_PICKER_CAPABILITY_TTL_SECS,
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, value);
    }
}

fn clear_picker_capability_cookie(headers: &mut HeaderMap) {
    let secure = if picker_cookie_secure() {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Lax; Max-Age=0; \
         Expires=Thu, 01 Jan 1970 00:00:00 GMT{secure}",
        name = INSTALL_PICKER_CAPABILITY_COOKIE,
        path = INSTALL_PICKER_CAPABILITY_PATH,
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, value);
    }
}

fn picker_cookie_secure() -> bool {
    if let Ok(value) = std::env::var("DJINN_COOKIE_SECURE") {
        matches!(value.as_str(), "true" | "1" | "TRUE" | "yes")
    } else {
        public_url().starts_with("https://")
    }
}

/// Authenticate the picker without creating a Djinn user or browser session.
/// The cookie is a bearer copy of the still-pending install continuation;
/// validation uses the server-held nonce and constant-time comparison.
async fn require_picker_capability(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, Response> {
    let Some(capability) = extract_cookie(headers, INSTALL_PICKER_CAPABILITY_COOKIE) else {
        return Err(picker_forbidden_response(false));
    };
    if !state
        .validate_pending_install_continuation(&capability)
        .await
    {
        return Err(picker_forbidden_response(true));
    }
    Ok(capability)
}

fn picker_forbidden_response(clear_cookie: bool) -> Response {
    let mut response = (
        StatusCode::FORBIDDEN,
        "A valid installation-picker capability is required. Restart the correlated GitHub App \
         setup/install flow.",
    )
        .into_response();
    if clear_cookie {
        clear_picker_capability_cookie(response.headers_mut());
    }
    response
}

/// Enforce the public picker's one-shot setup contract.
///
/// A database read failure is fail-closed: continuing could replace a binding
/// that exists but could not be read. Callers run this before any GitHub fetch,
/// while POST's atomic create closes the concurrent-write race.
async fn require_unbound_picker(state: &AppState) -> Result<(), Response> {
    match OrgConfigRepository::new(state.db().clone()).get().await {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(already_bound_response()),
        Err(error) => {
            tracing::error!(
                error = %error,
                "installation picker: failed to check existing org_config",
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to verify whether this deployment is already bound",
            )
                .into_response())
        }
    }
}

fn already_bound_response() -> Response {
    (
        StatusCode::CONFLICT,
        "This deployment already has a GitHub installation binding. The public setup picker \
         is only available during initial setup.",
    )
        .into_response()
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
        let resp = list_installations(State(state), HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_installation_returns_503_when_app_unconfigured() {
        let state = test_helpers::test_app_state_in_memory().await;
        let resp = select_installation(
            State(state),
            HeaderMap::new(),
            Json(SelectRequest {
                installation_id: 42,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    async fn configured_state() -> AppState {
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
        state
    }

    fn picker_headers(capability: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{INSTALL_PICKER_CAPABILITY_COOKIE}={capability}"))
                .unwrap(),
        );
        headers
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn picker_get_and_post_reject_requests_without_capability() {
        let state = configured_state().await;

        let get_response = list_installations(State(state.clone()), HeaderMap::new()).await;
        assert_eq!(get_response.status(), StatusCode::FORBIDDEN);

        let post_response = select_installation(
            State(state.clone()),
            HeaderMap::new(),
            Json(SelectRequest {
                installation_id: 42,
            }),
        )
        .await;
        assert_eq!(post_response.status(), StatusCode::FORBIDDEN);
        assert!(
            OrgConfigRepository::new(state.db().clone())
                .get()
                .await
                .unwrap()
                .is_none(),
            "an unauthenticated request must not win the first-binding race"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_installation_rejects_zero_id_after_capability_validation() {
        let state = configured_state().await;
        let capability = "picker-zero-id";
        state
            .set_pending_install_continuation_for_tests(Some(capability.into()))
            .await;

        let resp = select_installation(
            State(state),
            picker_headers(capability),
            Json(SelectRequest { installation_id: 0 }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authorized_picker_capability_binds_once_and_is_consumed() {
        let state = configured_state().await;
        let capability = "authorized-picker-capability";
        state
            .set_pending_install_continuation_for_tests(Some(capability.into()))
            .await;
        *APP_INSTALLATIONS_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(vec![InstallationSummary {
            installation_id: 42,
            account_login: "acme".into(),
            account_id: 9001,
            account_type: "Organization".into(),
            repository_selection: "selected".into(),
            html_url: "https://github.test/installations/42".into(),
        }]));

        let response = select_installation(
            State(state.clone()),
            picker_headers(capability),
            Json(SelectRequest {
                installation_id: 42,
            }),
        )
        .await;
        *APP_INSTALLATIONS_RESULT_OVERRIDE.lock().unwrap() = None;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|cookie| {
                    cookie.starts_with(&format!("{INSTALL_PICKER_CAPABILITY_COOKIE}="))
                        && cookie.contains("Path=/api/github/installations")
                        && cookie.contains("Max-Age=0")
                }),
            "successful binding must clear the path-scoped picker capability"
        );
        assert!(
            !state.has_pending_install_continuation().await,
            "successful binding must consume the server-held capability"
        );
        let binding = OrgConfigRepository::new(state.db().clone())
            .get()
            .await
            .unwrap()
            .expect("authorized picker must persist the binding");
        assert_eq!(binding.installation_id, 42);
        assert_eq!(binding.github_org_login, "acme");
    }

    async fn configured_state_with_binding() -> AppState {
        use std::sync::Arc;

        let state = test_helpers::test_app_state_in_memory().await;
        let cfg = djinn_provider::github_app::AppConfig {
            app_id: 1,
            slug: "djinn".into(),
            client_id: "Iv1.x".into(),
            client_secret: "y".into(),
            // Deliberately invalid: a missing one-shot guard would attempt to
            // mint a JWT and produce 502 rather than the expected 409.
            pem: "not-a-private-key".into(),
            webhook_secret: "w".into(),
            public_url: "http://127.0.0.1:8372".into(),
        };
        state.set_app_config(Some(Arc::new(cfg))).await;
        OrgConfigRepository::new(state.db().clone())
            .set(NewOrgConfig {
                github_org_id: 10,
                github_org_login: "already-bound",
                app_id: 1,
                installation_id: 20,
            })
            .await
            .unwrap();
        state
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_installations_returns_409_when_already_bound_before_github_fetch() {
        let state = configured_state_with_binding().await;

        let resp = list_installations(State(state), HeaderMap::new()).await;

        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_installation_returns_409_without_overwriting_existing_binding() {
        let state = configured_state_with_binding().await;

        let resp = select_installation(
            State(state.clone()),
            HeaderMap::new(),
            Json(SelectRequest {
                installation_id: 42,
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let binding = OrgConfigRepository::new(state.db().clone())
            .get()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.github_org_login, "already-bound");
        assert_eq!(binding.installation_id, 20);
    }

    fn installation(id: u64, account_type: &str) -> InstallationSummary {
        InstallationSummary {
            installation_id: id,
            account_login: format!("account-{id}"),
            account_id: id,
            account_type: account_type.into(),
            repository_selection: "selected".into(),
            html_url: format!("https://github.test/installations/{id}"),
        }
    }

    #[test]
    fn picker_filters_personal_and_unknown_account_types_by_policy() {
        let without_users = selectable_installations(
            vec![
                installation(1, "Organization"),
                installation(2, "User"),
                installation(3, "Bot"),
            ],
            false,
        );
        assert_eq!(
            without_users
                .iter()
                .map(|entry| entry.installation_id)
                .collect::<Vec<_>>(),
            vec![1]
        );

        let with_users = selectable_installations(
            vec![
                installation(1, "Organization"),
                installation(2, "User"),
                installation(3, "Bot"),
            ],
            true,
        );
        assert_eq!(
            with_users
                .iter()
                .map(|entry| entry.installation_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
}
