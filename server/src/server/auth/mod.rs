// djinn:allow-oversize — self-setup gate + boot-token + existing auth routes; split when touched substantively.
//! GitHub App user-to-server OAuth HTTP routes (`/auth/*`).
//!
//! Implements the browser redirect flow used by the web client to force users
//! to sign in. This is now the GitHub **App**'s user-to-server OAuth — not
//! the classic OAuth App flow. Key differences:
//!   * No `scope` parameter. GitHub App permissions come from the App's
//!     declared manifest, not from OAuth scopes.
//!   * The user token is retained so the server can look up which
//!     installations the user can see (`GET /user/installations`); all
//!     repo I/O goes through installation tokens (see
//!     `djinn_provider::github_app`).
//!
//! Environment variables:
//!   * `GITHUB_APP_CLIENT_ID` — GitHub App client id (required).
//!   * `GITHUB_APP_CLIENT_SECRET` — GitHub App client secret (required).
//!   * `GITHUB_APP_SLUG` — App slug, used when `?install=1` is passed to
//!     redirect to the install page post-auth.
//!   * `DJINN_PUBLIC_URL` — Public base URL used to build the OAuth
//!     callback (defaults to `http://127.0.0.1:8372`).
//!   * `DJINN_COOKIE_SECURE` — `true` to force `Secure` on the session
//!     cookie.
//!
//! The flow:
//!   1. `GET /auth/github/start?redirect=<path>` — mint a random `state` value,
//!      stash it in a cookie alongside the requested post-login redirect
//!      (`djinn_oauth_state`), 302 to GitHub's `/login/oauth/authorize`.
//!   2. `GET /auth/github/callback?code=&state=` — validate the state cookie,
//!      POST to `/login/oauth/access_token` to swap the code for an access
//!      token, fetch `/user` for the identity, insert a row into
//!      `user_auth_sessions`, set the `djinn_session` cookie, 302 to the
//!      caller-requested redirect (default `/`).
//!   3. `GET /auth/me` — look up the session row, return the identity.
//!   4. `POST /auth/logout` — delete the session row, clear the cookie.
//!
//! ## Self-setup flow
//!
//! When `DJINN_ENABLE_SELF_SETUP=true` and no usable GitHub App credentials
//! exist, the server generates a one-time boot token at startup and logs a
//! setup URL containing the raw token. The setup flow is:
//!
//!   1. `GET /auth/github/create-app?setup_token=<raw>` — exchange the
//!      single-use boot token for a short-lived `djinn_setup_session` cookie,
//!      then 303-redirect to `/auth/github/create-app` (clean URL, no token).
//!   2. `GET /auth/github/create-app` (with valid setup session) — proceed
//!      with manifest creation. *(Manifest exchange implemented by the
//!      follow-up task.)*
//!   3. `GET /auth/github/app-manifest-callback` — handle the manifest-code
//!      exchange from GitHub. *(Implemented by the follow-up task.)*

pub(crate) mod boot_token;

use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use reqwest::Client;
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use djinn_db::{
    CreateUserAuthSession, NewOrgConfig, OrgConfigRepository, SessionAuthRepository, UserRepository,
};
use djinn_provider::github_app::jwt::mint_app_jwt_anyhow;
use djinn_provider::github_server::{AppInstallation, GitHubServerClient};
use djinn_provider::oauth::github_app_user::{self, GithubUserTokens};

pub(super) const SESSION_COOKIE: &str = "djinn_session";
const OAUTH_STATE_COOKIE: &str = "djinn_oauth_state";
pub(super) const DEFAULT_PUBLIC_URL: &str = "http://127.0.0.1:8372";
const SESSION_TTL_SECS: i64 = 60 * 60 * 24 * 30; // 30 days
const STATE_COOKIE_TTL_SECS: i64 = 60 * 10; // 10 minutes
/// CSRF state cookie for the GitHub App manifest creation flow.
const MANIFEST_STATE_COOKIE: &str = "djinn_app_manifest_state";

/// Cookie name for the short-lived setup session established after a
/// successful boot-token exchange.
pub(crate) const SETUP_SESSION_COOKIE: &str = "djinn_setup_session";
/// Setup session cookie path scope — limits the cookie to setup routes.
const SETUP_SESSION_PATH: &str = "/auth/github";
/// Setup session TTL: 15 minutes.
const SETUP_SESSION_TTL_SECS: i64 = 60 * 15;

/// Read a GitHub App OAuth client id/secret from the environment.
///
/// The legacy `GITHUB_OAUTH_CLIENT_ID` / `GITHUB_OAUTH_CLIENT_SECRET`
/// fallbacks were retired with the GitHub App finalization — only the
/// App-native env var names are honoured going forward.
fn read_github_app_oauth_env(primary: &str) -> Option<String> {
    std::env::var(primary).ok().filter(|v| !v.is_empty())
}

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/me", get(me))
        .route("/auth/config", get(config))
        .route("/auth/github/start", get(github_start))
        .route("/auth/github/callback", get(github_callback))
        .route("/auth/github/app-setup-callback", get(app_setup_callback))
        // Self-setup routes: gated by DJINN_ENABLE_SELF_SETUP + no usable
        // credentials. When the gate is closed these return 404.
        .route("/auth/github/create-app", get(create_app))
        .route(
            "/auth/github/app-manifest-callback",
            get(app_manifest_callback),
        )
        .route("/auth/logout", post(logout))
        .route("/setup/status", get(setup_status))
        // Auth/setup responses reflect live deployment + session state. Without
        // an explicit directive browsers apply heuristic freshness and can
        // hand `fetch()` a stale body — e.g. the UI kept rendering the "App
        // not configured" screen after the operator dropped the Secret.
        .layer(middleware::from_fn(no_store))
}

async fn no_store(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

#[derive(Serialize)]
struct ConfigResponse {
    configured: bool,
    missing: Vec<&'static str>,
    setup_doc_url: &'static str,
    /// Whether the self-setup flow is available for the operator to create
    /// a new GitHub App via the manifest flow. Only `true` when
    /// `DJINN_ENABLE_SELF_SETUP=true` AND no usable credentials exist.
    #[serde(default)]
    self_setup_available: bool,
}

/// Report whether the GitHub App is configured (env-only after the K8s
/// migration). Used by the UI to decide between sign-in and a static
/// "App not configured" notice.
async fn config(State(state): State<AppState>) -> Json<ConfigResponse> {
    let active = state.app_config().await;
    let mut missing: Vec<&'static str> = Vec::new();

    if active.is_none() {
        // Surface a useful "missing" list so the operator can spot which
        // env var (or Helm secret key) is unset.
        let required = [
            "GITHUB_APP_CLIENT_ID",
            "GITHUB_APP_CLIENT_SECRET",
            "GITHUB_APP_ID",
            "GITHUB_APP_SLUG",
        ];
        for k in required {
            if read_github_app_oauth_env(k).is_none() {
                missing.push(k);
            }
        }
        let private_key_set = read_github_app_oauth_env("GITHUB_APP_PRIVATE_KEY").is_some()
            || read_github_app_oauth_env("GITHUB_APP_PRIVATE_KEY_PATH").is_some();
        if !private_key_set {
            missing.push("GITHUB_APP_PRIVATE_KEY");
        }
    }

    let self_setup_available = setup_available(active.is_some());

    Json(ConfigResponse {
        configured: active.is_some(),
        missing,
        setup_doc_url: "https://github.com/djinnos/djinn/blob/main/docs/GITHUB_APP_SETUP.md",
        self_setup_available,
    })
}

use std::sync::atomic::{AtomicI8, Ordering};

// ─── Self-setup gate helpers ──────────────────────────────────────────────────

/// Test-only override for `self_setup_enabled()`.
/// -1 = no override (use env var), 0 = forced false, 1 = forced true.
static SELF_SETUP_OVERRIDE: AtomicI8 = AtomicI8::new(-1);

/// Async mutex that serialises access to the self-setup override during tests.
/// Using `tokio::sync::Mutex` avoids the clippy `await_holding_lock` lint that
/// fires for `std::sync::Mutex` guards held across `.await` points.
#[cfg(test)]
static SELF_SETUP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acquire the async test lock, set the override, and return the guard.
/// The override stays set until the guard is dropped (end of test).
#[cfg(test)]
async fn with_self_setup_override(value: Option<bool>) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = SELF_SETUP_TEST_LOCK.lock().await;
    let v = match value {
        None => -1,
        Some(true) => 1,
        Some(false) => 0,
    };
    SELF_SETUP_OVERRIDE.store(v, Ordering::SeqCst);
    guard
}

/// Whether the `DJINN_ENABLE_SELF_SETUP` environment variable is set to true.
pub(crate) fn self_setup_enabled() -> bool {
    match SELF_SETUP_OVERRIDE.load(Ordering::SeqCst) {
        0 => false,
        1 => true,
        _ => std::env::var("DJINN_ENABLE_SELF_SETUP")
            .map(|v| matches!(v.as_str(), "true" | "1" | "TRUE"))
            .unwrap_or(false),
    }
}

/// Whether the self-setup UI/flow should be offered: the gate is enabled AND
/// no usable GitHub App credentials exist yet.
fn setup_available(has_usable_credentials: bool) -> bool {
    self_setup_enabled() && !has_usable_credentials
}

/// Set a `djinn_setup_session` cookie scoped to the setup route prefix.
///
/// Cookie properties:
/// - HttpOnly, SameSite=Lax
/// - Path-scoped to `/auth/github`
/// - Secure when `DJINN_PUBLIC_URL` is HTTPS
/// - Expires after 15 minutes
fn set_setup_cookie(headers: &mut HeaderMap, value: &str) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie = format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
        name = SETUP_SESSION_COOKIE,
        path = SETUP_SESSION_PATH,
        max_age = SETUP_SESSION_TTL_SECS,
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

/// Extract and validate a `djinn_setup_session` cookie from the request
/// headers. Returns `Some(session_token)` when the cookie is present and
/// matches the session token stored by a prior `exchange_boot_token` call.
async fn extract_setup_session(headers: &HeaderMap, state: &AppState) -> Option<String> {
    let token = extract_cookie(headers, SETUP_SESSION_COOKIE)?;
    if token.is_empty() {
        return None;
    }
    // Validate against the stored session token so an arbitrary cookie value
    // is rejected.  The token was stored by `exchange_boot_token`; it remains
    // valid until cleared after credential persistence.
    state
        .validate_setup_session_token(&token)
        .await
        .then_some(token)
}

/// Clear the setup session cookie.
/// Used after credential persistence to clear the setup session.
fn clear_setup_cookie(headers: &mut HeaderMap) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie = format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Lax; Max-Age=0; \
         Expires=Thu, 01 Jan 1970 00:00:00 GMT{secure}",
        name = SETUP_SESSION_COOKIE,
        path = SETUP_SESSION_PATH,
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

/// Guard for setup routes: returns `Some(404 response)` when the self-setup
/// gate is closed (disabled or usable credentials already exist), or `None`
/// when setup is available and the handler should proceed.
async fn setup_route_guard(state: &AppState) -> Option<Response> {
    let has_usable = state.app_config().await.is_some();
    if !setup_available(has_usable) {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    None
}

// ─── Extractor ────────────────────────────────────────────────────────────────

/// A user authenticated via a valid `djinn_session` cookie.
///
/// Wire this into handlers by calling [`authenticate`] with the incoming
/// headers + [`AppState`]. A future iteration can graduate this into an
/// [`axum::extract::FromRequestParts`] impl once the shape of the `Option`
/// vs. required variants stabilises.
#[derive(Debug, Clone, Serialize)]
pub struct AuthenticatedUser {
    pub id: String,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    /// Admin privilege, sourced from the joined `users` row. Gates global
    /// runtime settings and the Users admin page.
    pub is_admin: bool,
    /// Proposal capability role: `proposer` | `pm` | `engineer`.
    pub role: String,
    /// The raw cookie token, for callers that want to refresh or revoke it.
    #[serde(skip)]
    pub session_token: String,
    /// The GitHub user access token, used to call user-scoped GitHub APIs
    /// (e.g. `GET /user/installations`). Never serialised to clients.
    #[serde(skip)]
    pub github_access_token: String,
}

/// Resolve a request's `djinn_session` cookie into an [`AuthenticatedUser`],
/// if any. Returns `Ok(None)` for the unauthenticated case; returns `Err`
/// only on database errors.
///
/// `id` is the stable `users.id` UUID surrogate, sourced from the joined
/// `users` row — not the denormalised GitHub-numeric column that lived on
/// `user_auth_sessions` before migration 22.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> djinn_db::Result<Option<AuthenticatedUser>> {
    let Some(token) = extract_cookie(headers, SESSION_COOKIE) else {
        return Ok(None);
    };
    let repo = SessionAuthRepository::new(state.db().clone());
    let Some((session, user)) = repo.get_by_token_with_user(&token).await? else {
        return Ok(None);
    };
    if session_expired(&session.expires_at) {
        // Best-effort cleanup; ignore errors.
        let _ = repo.delete_by_token(&token).await;
        return Ok(None);
    }
    Ok(Some(AuthenticatedUser {
        id: user.id,
        login: user.github_login,
        name: user.github_name,
        avatar_url: user.github_avatar_url,
        is_admin: user.is_admin,
        role: user.role,
        session_token: session.token,
        github_access_token: session.github_access_token,
    }))
}

/// Admin gate for REST handlers. Resolves the `djinn_session` cookie and
/// requires the resulting user to be an admin. Returns the authenticated admin
/// on success, or an `(status, message)` error suitable for `?` in handlers
/// that return `Result<_, (StatusCode, String)>`:
/// - `401 UNAUTHORIZED` when there is no valid session,
/// - `403 FORBIDDEN` when the session is valid but the user is not an admin,
/// - `500 INTERNAL_SERVER_ERROR` on a database error.
///
/// Unlike the MCP-tool gate (`tools::acting_user::require_admin`, which allows
/// the no-user "trusted" path for background agents), this never grants access
/// without a valid session — these REST endpoints are client-facing, so a
/// missing cookie must mean unauthenticated, not admin.
pub async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, (StatusCode, String)> {
    match authenticate(state, headers).await {
        Ok(Some(user)) if user.is_admin => Ok(user),
        Ok(Some(_)) => Err((
            StatusCode::FORBIDDEN,
            "admin privileges are required".to_string(),
        )),
        Ok(None) => Err((
            StatusCode::UNAUTHORIZED,
            "authentication required".to_string(),
        )),
        Err(e) => {
            tracing::error!(error = %e, "admin gate: auth lookup failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct MeResponse {
    id: String,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    /// Whether this user is an admin (gates the global settings + Users page).
    is_admin: bool,
    /// Proposal capability role: `proposer` | `pm` | `engineer`.
    role: String,
    /// GitHub org this deployment is locked to. Surfaced so the web client can
    /// show "signed in as <login> on <org>" without a second round-trip.
    /// `None` when the deployment hasn't finished the manifest flow yet.
    org_login: Option<String>,
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authenticate(&state, &headers).await {
        Ok(Some(user)) => {
            let org_login = org_login_for_response(&state).await;
            Json(MeResponse {
                id: user.id,
                login: user.login,
                name: user.name,
                avatar_url: user.avatar_url,
                is_admin: user.is_admin,
                role: user.role,
                org_login,
            })
            .into_response()
        }
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "auth /me: db error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Best-effort read of `org_config.github_org_login`. Errors are logged and
/// swallowed — we'd rather surface the user identity with `org_login: null`
/// than 500 the `/auth/me` endpoint over a transient DB blip.
async fn org_login_for_response(state: &AppState) -> Option<String> {
    let repo = OrgConfigRepository::new(state.db().clone());
    match repo.get().await {
        Ok(Some(cfg)) => Some(cfg.github_org_login),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "auth /me: org_config lookup failed");
            None
        }
    }
}

#[derive(Deserialize)]
struct StartQuery {
    #[serde(default)]
    redirect: Option<String>,
    /// When `install=1`, after user auth completes we 302 the browser to the
    /// GitHub App's install page instead of the requested `redirect`. Useful
    /// for a "Connect" button when the user has no installations yet.
    #[serde(default)]
    install: Option<String>,
}

async fn github_start(State(state): State<AppState>, Query(q): Query<StartQuery>) -> Response {
    let active = state.app_config().await;
    let client_id = match active
        .as_ref()
        .map(|c| c.client_id.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| read_github_app_oauth_env("GITHUB_APP_CLIENT_ID"))
    {
        Some(v) => v,
        None => {
            tracing::error!("auth /github/start: GITHUB_APP_CLIENT_ID not set");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "GitHub App OAuth is not configured",
            )
                .into_response();
        }
    };
    let redirect = sanitize_redirect(q.redirect.as_deref());
    let want_install = matches!(q.install.as_deref(), Some("1") | Some("true"));
    let state_token = random_token_b64();
    // Encode `state_token|want_install|redirect` in the state cookie so the
    // callback can verify all three without database writes. The `i1`/`i0`
    // prefix encodes the install flag.
    let install_flag = if want_install { "i1" } else { "i0" };
    let cookie_value = format!("{state_token}|{install_flag}|{redirect}");

    let callback = format!("{}/auth/github/callback", public_url());
    // GitHub Apps do not use OAuth scopes — permissions come from the App's
    // manifest. We pass `allow_signup=true` so new GH users can still sign
    // in without bouncing to signup first.
    let auth_url = format!(
        "https://github.com/login/oauth/authorize?client_id={cid}&redirect_uri={cb}&state={st}&allow_signup=true",
        cid = urlencode(&client_id),
        cb = urlencode(&callback),
        st = urlencode(&state_token),
    );

    let mut headers = HeaderMap::new();
    set_cookie(
        &mut headers,
        OAUTH_STATE_COOKIE,
        &cookie_value,
        STATE_COOKIE_TTL_SECS,
    );
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&auth_url).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    (StatusCode::FOUND, headers).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// Present when GitHub redirects here after an App *install* (because
    /// the App has no explicit `setup_url`). We recognise it and bounce
    /// the user to the web app — no OAuth exchange, no session creation.
    installation_id: Option<String>,
    setup_action: Option<String>,
}

async fn github_callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Response {
    // Install-completion redirect routed to `callback_urls` instead of
    // `setup_url`. Happens either when the manifest has no `setup_url`, or
    // when `request_oauth_on_install` was set to `true` (GitHub then
    // bypasses `setup_url`). Forward to `app_setup_callback` so the
    // `installation_id` actually gets captured and `org_config` is written
    // — bouncing straight home (the old behaviour) silently lost the
    // binding and left the deployment half-configured.
    if let Some(installation_id) = q.installation_id.as_ref()
        && q.setup_action.as_deref() == Some("install")
    {
        let mut resp_headers = HeaderMap::new();
        let target = format!(
            "{}/auth/github/app-setup-callback?installation_id={}&setup_action=install",
            public_url().trim_end_matches('/'),
            urlencode(installation_id),
        );
        resp_headers.insert(
            header::LOCATION,
            HeaderValue::from_str(&target).unwrap_or_else(|_| HeaderValue::from_static("/")),
        );
        return (StatusCode::FOUND, resp_headers).into_response();
    }

    let (code, state_param) = match (q.code, q.state) {
        (Some(c), Some(s)) if !c.is_empty() && !s.is_empty() => (c, s),
        _ => return (StatusCode::BAD_REQUEST, "missing code or state").into_response(),
    };

    let Some(cookie_raw) = extract_cookie(&headers, OAUTH_STATE_COOKIE) else {
        return (StatusCode::BAD_REQUEST, "missing state cookie").into_response();
    };
    // Cookie format: `<state>|i0|<redirect>` or `<state>|i1|<redirect>`.
    // Legacy format (`<state>|<redirect>`) is accepted for in-flight
    // sign-ins during the rollout.
    let mut parts = cookie_raw.splitn(3, '|');
    let cookie_state = parts.next().unwrap_or("").to_string();
    let (want_install, redirect) = match (parts.next(), parts.next()) {
        (Some("i1"), Some(r)) => (true, r.to_string()),
        (Some("i0"), Some(r)) => (false, r.to_string()),
        // Legacy 2-part encoding.
        (Some(r), None) => (false, r.to_string()),
        _ => (false, "/".to_string()),
    };
    if !constant_time_eq(cookie_state.as_bytes(), state_param.as_bytes()) {
        return (StatusCode::BAD_REQUEST, "state mismatch").into_response();
    }

    let active = state.app_config().await;
    let (client_id, client_secret) = match active.as_ref() {
        Some(cfg) if !cfg.client_id.is_empty() && !cfg.client_secret.is_empty() => {
            (cfg.client_id.clone(), cfg.client_secret.clone())
        }
        _ => (
            read_github_app_oauth_env("GITHUB_APP_CLIENT_ID").unwrap_or_default(),
            read_github_app_oauth_env("GITHUB_APP_CLIENT_SECRET").unwrap_or_default(),
        ),
    };
    if client_id.is_empty() || client_secret.is_empty() {
        tracing::error!("auth callback: GitHub App OAuth env vars missing");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "GitHub App OAuth is not configured",
        )
            .into_response();
    }

    // 1. Exchange code for access token + (optional) refresh token.
    let callback_url = format!("{}/auth/github/callback", public_url());
    let tokens = match github_app_user::exchange_code(
        &client_id,
        &client_secret,
        &code,
        &callback_url,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "auth callback: token exchange failed");
            return (StatusCode::BAD_GATEWAY, "token exchange failed").into_response();
        }
    };
    let access_token = tokens.access_token.clone();

    // 2. Fetch /user to build the identity.
    let user = match fetch_github_user(&access_token).await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "auth callback: /user fetch failed");
            return (StatusCode::BAD_GATEWAY, "failed to fetch GitHub user").into_response();
        }
    };

    // 3. Phase 2: enforce "one deployment = one GitHub org". Look up the
    //    deployment's locked org; if absent the deployment isn't set up.
    //
    //    Exception: the bootstrap flow (`want_install=true` on a fresh
    //    deployment) routes through this handler *before* `org_config` can
    //    possibly exist — the install redirect we emit at the end of this
    //    function is what lets GitHub invoke `app_setup_callback`, which is
    //    what writes `org_config`. If we rejected here, the setup flow could
    //    never complete. So in that case we skip the org checks and create
    //    the session; `app_setup_callback` still writes the binding on its
    //    own authority (App JWT against `GET /app/installations/{id}`).
    let org_repo = OrgConfigRepository::new(state.db().clone());
    let org_cfg = match org_repo.get().await {
        Ok(Some(cfg)) => Some(cfg),
        Ok(None) if want_install => {
            tracing::info!(
                user = %user.login,
                "auth callback: bootstrap flow — skipping org_config/membership checks",
            );
            None
        }
        Ok(None) => {
            tracing::warn!("auth callback: rejecting login — deployment has no org_config yet");
            return (
                StatusCode::PRECONDITION_FAILED,
                "Djinn is not configured yet. The deployment owner must complete \
                 the GitHub App manifest flow before anyone can sign in.",
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "auth callback: org_config read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // 4. Verify the signed-in user is an active member of the locked org.
    //    GitHub's `/user/memberships/orgs/{org}` is authenticated with the
    //    *user* token we just got; it returns `state: "active"|"pending"` on
    //    2xx and 404 for non-members. We treat only `state == "active"` as
    //    a pass; pending invites still count as "not a member".
    //
    //    Skipped during bootstrap — there's no org yet to check membership
    //    against; `app_setup_callback` validates the installation target
    //    separately.
    if let Some(cfg) = &org_cfg {
        match check_org_membership(&access_token, &cfg.github_org_login).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    user = %user.login,
                    org = %cfg.github_org_login,
                    "auth callback: rejecting non-member",
                );
                let body = format!(
                    "Access denied. This deployment is locked to the GitHub org '{org}', \
                     and the GitHub account '{login}' is not an active member.",
                    org = cfg.github_org_login,
                    login = user.login,
                );
                return (StatusCode::FORBIDDEN, body).into_response();
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    org = %cfg.github_org_login,
                    "auth callback: membership check failed",
                );
                return (
                    StatusCode::BAD_GATEWAY,
                    "failed to verify GitHub org membership",
                )
                    .into_response();
            }
        }
    }

    // 5. Upsert the persistent `users` row → stable surrogate `users.id`.
    let users_repo = UserRepository::new(state.db().clone());
    let user_row = match users_repo
        .upsert_from_github(
            user.id as i64,
            &user.login,
            user.name.as_deref(),
            user.avatar_url.as_deref(),
        )
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "auth callback: users upsert failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Bootstrap admin: the first user to sign in (when no admin exists yet)
    // becomes admin. This is the only automatic admin grant — further changes
    // are manual `UPDATE users SET is_admin = …`. Best-effort: a failure here
    // must not block sign-in (the next login retries while admin_count is 0).
    if !user_row.is_admin {
        match users_repo.admin_count().await {
            Ok(0) => {
                if let Err(e) = users_repo.set_admin_status(&user_row.id, true).await {
                    tracing::warn!(error = %e, user_id = %user_row.id, "auth callback: failed to stamp bootstrap admin");
                } else {
                    tracing::info!(user_id = %user_row.id, login = %user.login, "auth callback: stamped first user as admin");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "auth callback: admin_count check failed; skipping bootstrap");
            }
        }
    }

    // 6. Persist a new session row, linked to the users table via `user_fk`.
    //
    //    Two independent deadlines on the row:
    //    * `expires_at` — browser session cookie TTL (30d). The user stays
    //      signed in this long regardless of GitHub-side rotations.
    //    * `github_access_token_expires_at` + `github_refresh_token_*` —
    //      GitHub's deadlines, taken straight from `expires_in` /
    //      `refresh_token_expires_in` in the OAuth response. NULL when the
    //      App is configured with non-expiring user tokens.
    let token = random_token_b64();
    let expires_at = rfc3339_in(SESSION_TTL_SECS);
    let GithubUserTokens {
        access_token: _,
        expires_in,
        refresh_token,
        refresh_token_expires_in,
    } = tokens;
    let access_expires_at = expires_in.map(rfc3339_in);
    let refresh_expires_at = refresh_token_expires_in.map(rfc3339_in);
    let repo = SessionAuthRepository::new(state.db().clone());
    if let Err(e) = repo
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user_row.id,
            github_login: &user.login,
            github_name: user.name.as_deref(),
            github_avatar_url: user.avatar_url.as_deref(),
            github_access_token: &access_token,
            github_access_token_expires_at: access_expires_at.as_deref(),
            github_refresh_token: refresh_token.as_deref(),
            github_refresh_token_expires_at: refresh_expires_at.as_deref(),
            expires_at: &expires_at,
        })
        .await
    {
        tracing::error!(error = %e, "auth callback: failed to persist session");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // 4. Build redirect response with cookies.
    //    If `?install=1` was passed to /start, we send the user to the App's
    //    install page instead of the app home. Otherwise, honour the
    //    site-local redirect.
    let mut resp_headers = HeaderMap::new();
    set_cookie(&mut resp_headers, SESSION_COOKIE, &token, SESSION_TTL_SECS);
    clear_cookie(&mut resp_headers, OAUTH_STATE_COOKIE);
    let path = sanitize_redirect(Some(&redirect));
    let web_base = web_url();
    let local_fallback = format!("{}{}", web_base.trim_end_matches('/'), path);
    let location = if want_install {
        let slug = active
            .as_ref()
            .map(|c| c.slug.clone())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("GITHUB_APP_SLUG")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            });
        match slug {
            Some(s) => format!("https://github.com/apps/{}/installations/new", s.trim()),
            None => {
                tracing::warn!("auth callback: install=1 requested but GITHUB_APP_SLUG is unset");
                local_fallback
            }
        }
    } else {
        local_fallback
    };
    resp_headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    (StatusCode::FOUND, resp_headers).into_response()
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Resolve the cookie up front so we can also nuke any sibling sessions
    // for the same user (rotating workstations, leftover dev tabs, etc.).
    let cookie_token = extract_cookie(&headers, SESSION_COOKIE);
    let repo = SessionAuthRepository::new(state.db().clone());

    if let Some(token) = &cookie_token {
        // Best-effort look up the user behind this cookie so we can drop
        // every session for them, not just this one. Falls back to a
        // single-row delete if the lookup fails.
        let user_fk = match repo.get_by_token(token).await {
            Ok(Some(row)) => Some(row.user_fk),
            _ => None,
        };
        if let Some(user_fk) = user_fk {
            if let Err(e) = repo.delete_by_user_fk(&user_fk).await {
                tracing::warn!(
                    error = %e,
                    user_fk = %user_fk,
                    "auth /logout: failed to delete sessions by user_fk",
                );
            }
        } else if let Err(e) = repo.delete_by_token(token).await {
            tracing::warn!(error = %e, "auth /logout: failed to delete session row");
        }
    }
    tracing::info!(
        had_cookie = cookie_token.is_some(),
        "auth /logout: clearing browser session"
    );

    let mut resp_headers = HeaderMap::new();
    clear_cookie(&mut resp_headers, SESSION_COOKIE);
    // 200 + JSON body — some browsers and proxies treat 204 conservatively
    // and `fetch().json()` on the client expects a parseable body.
    resp_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    (StatusCode::OK, resp_headers, "{\"ok\":true}").into_response()
}

// ─── GitHub API helpers ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GhUser {
    id: u64,
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

async fn fetch_github_user(access_token: &str) -> Result<GhUser, String> {
    let user = GitHubServerClient::new().fetch_user(access_token).await?;
    Ok(GhUser {
        id: user.id,
        login: user.login,
        name: user.name,
        avatar_url: user.avatar_url,
    })
}

/// Verify `access_token` belongs to an **active** member of `org_login`.
///
/// Uses `GET /user/memberships/orgs/{org}`, the endpoint GitHub documents as
/// the canonical "am I in this org?" probe for user-to-server tokens.
/// Returns:
///   * `Ok(true)` — 200 response with `state == "active"`.
///   * `Ok(false)` — 404 (the user can't see the org), 403 (e.g. revoked),
///     or 200 with `state == "pending"` (invite not yet accepted). We
///     intentionally treat pending invites as non-members: the deployment
///     policy is "active members only". Any other non-success status is
///     surfaced as an error so callers can decide whether to 502.
///   * `Err(_)` — network or decode failure.
async fn check_org_membership(access_token: &str, org_login: &str) -> Result<bool, String> {
    GitHubServerClient::new()
        .check_org_membership(access_token, org_login)
        .await
}

// ─── Cookie + misc helpers ────────────────────────────────────────────────────

pub(super) fn public_url() -> String {
    std::env::var("DJINN_PUBLIC_URL").unwrap_or_else(|_| DEFAULT_PUBLIC_URL.to_string())
}

/// Where to send the browser after a completed OAuth/install flow.
///
/// Defaults to `DJINN_PUBLIC_URL`. Set `DJINN_WEB_URL` separately when the
/// web client is served on a different origin (e.g. Vite dev server on
/// `:1420` while the API server runs on `:8372`).
fn web_url() -> String {
    std::env::var("DJINN_WEB_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(public_url)
}

fn cookie_secure() -> bool {
    if let Ok(v) = std::env::var("DJINN_COOKIE_SECURE") {
        matches!(v.as_str(), "true" | "1" | "TRUE" | "yes")
    } else {
        public_url().starts_with("https://")
    }
}

pub(super) fn set_cookie(headers: &mut HeaderMap, name: &str, value: &str, max_age: i64) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie =
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}");
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

fn clear_cookie(headers: &mut HeaderMap, name: &str) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    // Belt-and-braces: pair `Max-Age=0` with a far-past `Expires`. Some
    // (older) Safari builds silently ignore `Max-Age` on responses they
    // treat as third-party — `Expires` is the original RFC 2109
    // mechanism every browser honours.
    let cookie = format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; \
         Expires=Thu, 01 Jan 1970 00:00:00 GMT{secure}",
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

pub(super) fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE).iter() {
        let Ok(s) = value.to_str() else { continue };
        for part in s.split(';') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=')
                && k == name
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub(super) fn random_token_b64() -> String {
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("SystemRandom available");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub(super) fn rfc3339_in(seconds: i64) -> String {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds);
    t.format(&Rfc3339).unwrap_or_else(|_| String::new())
}

pub(super) fn session_expired(expires_at: &str) -> bool {
    use time::format_description::well_known::Rfc3339;
    let Ok(expiry) = time::OffsetDateTime::parse(expires_at, &Rfc3339) else {
        // If we can't parse it, be safe and treat as expired.
        return true;
    };
    expiry <= time::OffsetDateTime::now_utc()
}

/// Only accept redirect targets that are site-local paths ("/..."). Prevents
/// open-redirect abuse where the attacker forges `?redirect=https://evil`.
fn sanitize_redirect(raw: Option<&str>) -> String {
    match raw {
        Some(p) if p.starts_with('/') && !p.starts_with("//") => p.to_string(),
        _ => "/".to_string(),
    }
}

pub(super) fn urlencode(s: &str) -> String {
    // Minimal percent-encoder for the handful of URL components we paste in
    // by hand. We avoid pulling in `urlencoding`/`percent-encoding` by only
    // encoding the characters that actually matter for query/value strings.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b;
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(c as char);
            }
            _ => out.push_str(&format!("%{:02X}", c)),
        }
    }
    out
}

pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── GitHub App install flow ──────────────────────────────────────────────────
//
// The legacy in-UI manifest auto-provision wizard is gone — App credentials
// are provisioned exclusively via the `djinn-github-app` Kubernetes Secret
// (see `server/docker/README.md`). The only endpoint that survives in this
// section is `GET /auth/github/app-setup-callback` (further down): GitHub
// posts the user there after they complete an App install on the target
// org, and we use that callback to bind `org_config`.

/// Query parameters for `GET /auth/github/app-setup-callback` — GitHub
/// appends `?installation_id=<N>&setup_action=install` after the user
/// completes (or requests) an installation via the App's install page.
#[derive(Deserialize)]
struct AppSetupQuery {
    installation_id: Option<String>,
    #[serde(default)]
    setup_action: Option<String>,
}

/// `GET /auth/github/app-setup-callback` — invoked by GitHub after the user
/// installs the App on an org (configured as the App's `setup_url`). We fetch
/// the installation's authoritative account info with the App JWT and write
/// the singleton `org_config` row that binds this deployment to that org.
///
/// Security note: the `installation_id` in the query is user-controllable,
/// so we do not trust query-derived org metadata. The binding is based on
/// the `account` returned by GitHub's `GET /app/installations/{id}` endpoint
/// authenticated with our App's JWT — which only succeeds for installations
/// of *this* App, and returns data GitHub computes from its own records.
async fn app_setup_callback(
    State(state): State<AppState>,
    Query(q): Query<AppSetupQuery>,
) -> Response {
    let installation_id: u64 = match q.installation_id.as_deref().and_then(|s| s.parse().ok()) {
        Some(id) if id > 0 => id,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "missing or invalid installation_id",
            )
                .into_response();
        }
    };
    // `setup_action` is usually "install" or "update". We don't gate on it —
    // any post-install hit with a valid installation_id should complete the
    // binding — but we log it for auditability.
    let action = q.setup_action.as_deref().unwrap_or("");

    let cfg = match state.app_config().await {
        Some(c) => c,
        None => {
            return (
                StatusCode::CONFLICT,
                "GitHub App credentials are not configured. Mount the \
                 djinn-github-app Kubernetes Secret (see \
                 server/docker/README.md) and restart the Pod.",
            )
                .into_response();
        }
    };

    // Resolve the installation authoritatively via the App JWT. This call
    // returns the target org's numeric id + login, which we need for the
    // org_config row.
    let installation = match fetch_installation_for_setup(installation_id).await {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(
                installation_id,
                error = %e,
                "app_setup_callback: fetch installation failed",
            );
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch installation {installation_id} from GitHub: {e}"),
            )
                .into_response();
        }
    };

    if !installation
        .account()
        .account_type
        .eq_ignore_ascii_case("Organization")
    {
        tracing::warn!(
            installation_id,
            account_type = %installation.account().account_type,
            account_login = %installation.account().login,
            "app_setup_callback: rejecting non-org installation",
        );
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "This deployment requires a GitHub *organization* installation, \
                 but installation {installation_id} is bound to account '{}' (type={}). \
                 Reinstall the App on an organization.",
                installation.account().login,
                installation.account().account_type,
            ),
        )
            .into_response();
    }

    let org_repo = OrgConfigRepository::new(state.db().clone());
    // Idempotency: if org_config already points at this installation, the
    // user probably double-clicked or reloaded — don't surface a confusing
    // 409, just redirect them home.
    if let Ok(Some(existing)) = org_repo.get().await
        && existing.installation_id as u64 == installation_id
        && existing.github_org_id as u64 == installation.account().id
    {
        tracing::info!(
            installation_id,
            action,
            "app_setup_callback: re-entry for already-bound org, redirecting home",
        );
        return redirect_to_web();
    }

    if let Err(e) = org_repo
        .set(NewOrgConfig {
            github_org_id: installation.account().id as i64,
            github_org_login: &installation.account().login,
            app_id: cfg.app_id as i64,
            installation_id: installation_id as i64,
        })
        .await
    {
        tracing::error!(
            error = %e,
            installation_id,
            account = %installation.account().login,
            "app_setup_callback: org_config set failed",
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to persist org binding. Check server logs.",
        )
            .into_response();
    }

    tracing::info!(
        installation_id,
        account = %installation.account().login,
        action,
        "app_setup_callback: org_config bound",
    );
    redirect_to_web()
}

/// Common post-success redirect — send the browser to the web client root.
fn redirect_to_web() -> Response {
    let mut resp_headers = HeaderMap::new();
    let target = format!("{}/", web_url().trim_end_matches('/'));
    resp_headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&target).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    (StatusCode::FOUND, resp_headers).into_response()
}

/// Fetch an installation's account info via the App JWT.
async fn fetch_installation_for_setup(installation_id: u64) -> Result<AppInstallation, String> {
    let jwt = mint_app_jwt_anyhow().map_err(|e| e.to_string())?;
    GitHubServerClient::new()
        .fetch_app_installation(&jwt, installation_id)
        .await
}

// ─── Setup-status endpoint (Phase 2) ──────────────────────────────────────────
//
// Public, no-auth endpoint so the web client can gate itself before even
// prompting the user to sign in. Returns enough information for the UI to
// decide between "show the big 'Create the GitHub App' button" and "show
// the usual sign-in flow".

#[derive(Serialize)]
struct SetupStatusResponse {
    /// True when either the GitHub App credentials are missing OR the
    /// deployment has no org binding in `org_config`. The UI uses this to
    /// gate sign-in, but combines it with `app_credentials_configured`
    /// to distinguish "operator must drop a Secret" from "user must
    /// pick an installation".
    needs_app_install: bool,
    /// True iff the GitHub App credentials (`GITHUB_APP_*` env / Secret)
    /// resolved on startup. When `true && needs_app_install == true`, the
    /// UI shows the in-app installation picker; when `false`, the UI shows
    /// the static "GitHub App not configured" runbook screen because the
    /// operator hasn't done their part yet.
    app_credentials_configured: bool,
    /// The org this deployment is locked to, once known. Sourced
    /// exclusively from the `org_config` DB row written by the picker.
    org_login: Option<String>,
}

async fn setup_status(State(state): State<AppState>) -> Json<SetupStatusResponse> {
    let app_cfg = state.app_config().await;
    let org_cfg = OrgConfigRepository::new(state.db().clone())
        .get()
        .await
        .ok()
        .flatten();
    let needs_app_install = app_cfg.is_none() || org_cfg.is_none();
    let app_credentials_configured = app_cfg.is_some();
    let org_login = org_cfg.map(|c| c.github_org_login);

    Json(SetupStatusResponse {
        needs_app_install,
        app_credentials_configured,
        org_login,
    })
}

// ─── Self-setup create-app + manifest-callback handlers ───────────────────────

/// Query parameters for `GET /auth/github/create-app`.
///
/// When `setup_token` is present, this is a boot-token exchange request.
/// Otherwise, the caller must present a valid `djinn_setup_session` cookie.
#[derive(Deserialize)]
struct CreateAppQuery {
    setup_token: Option<String>,
}

/// `GET /auth/github/create-app` — the self-setup entry point.
///
/// Two modes:
///
/// 1. **Token exchange** (`?setup_token=<raw>`): validates the single-use
///    boot token, atomically marks it consumed, sets a `djinn_setup_session`
///    cookie, and returns `303 Location: /auth/github/create-app` (clean URL,
///    no token in the redirect target).
///
/// 2. **Session-gated** (no query param, valid `djinn_setup_session` cookie):
///    the caller has already completed the token exchange. This path will
///    render an auto-submitting HTML form that POSTs the manifest JSON to
///    GitHub's App creation page, minting a CSRF state cookie along the way.
async fn create_app(
    State(state): State<AppState>,
    Query(q): Query<CreateAppQuery>,
    headers: HeaderMap,
) -> Response {
    // Gate: 404 when self-setup is disabled or usable credentials exist.
    if let Some(resp) = setup_route_guard(&state).await {
        return resp;
    }

    if let Some(raw_token) = q.setup_token.as_deref() {
        // Token exchange mode.
        if raw_token.is_empty() {
            return (StatusCode::BAD_REQUEST, "setup_token must not be empty").into_response();
        }

        let session_value = match state.exchange_boot_token(raw_token).await {
            crate::server::state::BootTokenExchangeResult::Ok(v) => v,
            crate::server::state::BootTokenExchangeResult::NotAvailable => {
                return (StatusCode::GONE, "setup token not available").into_response();
            }
            crate::server::state::BootTokenExchangeResult::InvalidOrUsed => {
                return (StatusCode::FORBIDDEN, "invalid or used setup token").into_response();
            }
        };

        // Set the setup session cookie and issue a clean 303 redirect.
        let mut resp_headers = HeaderMap::new();
        set_setup_cookie(&mut resp_headers, &session_value);
        resp_headers.insert(
            header::LOCATION,
            HeaderValue::from_static("/auth/github/create-app"),
        );
        return (StatusCode::SEE_OTHER, resp_headers).into_response();
    }

    // Session-gated mode: require a valid setup session cookie.
    if extract_setup_session(&headers, &state).await.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            "setup session required — provide ?setup_token=... for first access",
        )
            .into_response();
    }

    // Build the manifest and render an auto-submitting HTML form that
    // navigates to GitHub's App creation page. The CSRF state cookie
    // protects the manifest-code callback below.
    let public = public_url();
    let manifest = build_manifest_json(&public);
    let manifest_json = manifest.to_string();
    let csrf = random_token_b64();
    let manifest_escaped = html_attr_escape(&manifest_json);
    let csrf_escaped = html_attr_escape(&csrf);

    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Create Djinn GitHub App</title></head>\
         <body><p>Redirecting to GitHub to create the Djinn App…</p>\
         <form id=\"f\" method=\"post\" action=\"https://github.com/settings/apps/new?state={csrf}\">\
         <input type=\"hidden\" name=\"manifest\" value=\"{manifest}\" />\
         <noscript><button type=\"submit\">Continue to GitHub</button></noscript>\
         </form>\
         <script>document.getElementById('f').submit();</script>\
         </body></html>",
        csrf = csrf_escaped,
        manifest = manifest_escaped,
    );

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    set_cookie(
        &mut resp_headers,
        MANIFEST_STATE_COOKIE,
        &csrf,
        STATE_COOKIE_TTL_SECS,
    );
    (StatusCode::OK, resp_headers, html).into_response()
}

/// `GET /auth/github/app-manifest-callback` — handles the callback from
/// GitHub after the user completes the manifest creation flow.
///
/// Validates the CSRF manifest state cookie, exchanges the manifest code
/// via the GitHub API, persists the returned credentials under the
/// encrypted boundary, hot-reloads the active App config, clears setup
/// cookies/state, and redirects to the new App's install URL.
///
/// Requires a valid `djinn_setup_session` cookie.
#[derive(Deserialize)]
struct ManifestCallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn app_manifest_callback(
    State(state): State<AppState>,
    Query(q): Query<ManifestCallbackQuery>,
    headers: HeaderMap,
) -> Response {
    // Gate: 404 when self-setup is disabled or usable credentials exist.
    if let Some(resp) = setup_route_guard(&state).await {
        return resp;
    }

    // Require a valid setup session.
    if extract_setup_session(&headers, &state).await.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            "setup session required for manifest callback",
        )
            .into_response();
    }

    // Extract the manifest code and CSRF state from the callback query.
    let (code, state_param) = match (q.code, q.state) {
        (Some(c), Some(s)) if !c.is_empty() && !s.is_empty() => (c, s),
        _ => return (StatusCode::BAD_REQUEST, "missing code or state").into_response(),
    };

    // Validate the manifest CSRF state cookie against the query state.
    let Some(cookie_state) = extract_cookie(&headers, MANIFEST_STATE_COOKIE) else {
        return (StatusCode::BAD_REQUEST, "missing manifest state cookie").into_response();
    };
    if !constant_time_eq(cookie_state.as_bytes(), state_param.as_bytes()) {
        return (StatusCode::BAD_REQUEST, "state mismatch").into_response();
    }

    // Exchange the manifest code for the App credentials.
    let conversion = match exchange_manifest_code(&code).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "manifest callback: exchange failed");
            // Preserve retry ability: clear the manifest state cookie so
            // the user can restart the setup flow, but do NOT clear the
            // setup session cookie — they can retry from the create-app page.
            let mut resp_headers = HeaderMap::new();
            clear_cookie(&mut resp_headers, MANIFEST_STATE_COOKIE);
            return (
                StatusCode::BAD_GATEWAY,
                resp_headers,
                "manifest exchange failed — try again",
            )
                .into_response();
        }
    };

    // Build the AppConfig from the manifest conversion response.
    let public = public_url();
    let cfg = djinn_provider::github_app::AppConfig {
        app_id: conversion.id,
        slug: conversion.slug,
        client_id: conversion.client_id,
        client_secret: conversion.client_secret,
        pem: conversion.pem,
        webhook_secret: conversion.webhook_secret.unwrap_or_default(),
        public_url: public.clone(),
    };

    // Persist credentials under the encrypted boundary and hot-reload.
    match state.persist_and_reload_app_config(&cfg).await {
        Ok(_state) => {
            tracing::info!(
                app_id = cfg.app_id,
                slug = %cfg.slug,
                "manifest callback: credentials persisted and hot-reloaded"
            );
        }
        Err(e) => {
            tracing::error!(error = %e, "manifest callback: persistence failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    // Build the install URL for the newly created App.
    let install_url = cfg
        .install_url()
        .unwrap_or_else(|| format!("{}/", web_url().trim_end_matches('/')));

    // Clear all setup cookies/state: manifest CSRF + setup session.
    let mut resp_headers = HeaderMap::new();
    clear_cookie(&mut resp_headers, MANIFEST_STATE_COOKIE);
    clear_setup_cookie(&mut resp_headers);
    resp_headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&install_url).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    (StatusCode::FOUND, resp_headers).into_response()
}

// ─── Manifest helpers ────────────────────────────────────────────────────────

/// Build the manifest JSON object for a given public URL.
///
/// Pure function so tests can pin its shape. Requirements:
/// - App name prefix is `djinn-`
/// - `request_oauth_on_install: true`
/// - webhook inactive
/// - permissions exactly `contents: write` and `pull_requests: write`
pub(crate) fn build_manifest_json(public_url: &str) -> serde_json::Value {
    // GitHub automatically grants `metadata: read` as a base permission; we
    // only list the permissions we explicitly need.
    let permissions = serde_json::json!({
        "contents": "write",
        "pull_requests": "write",
    });
    serde_json::json!({
        "name": format!("djinn-{}", url_host(public_url).unwrap_or_else(|| "local".to_string())),
        "url": public_url,
        "hook_attributes": {
            "url": format!("{}/webhooks/github", public_url),
            "active": false,
        },
        "redirect_url": format!("{}/auth/github/app-manifest-callback", public_url),
        "callback_urls": [format!("{}/auth/github/callback", public_url)],
        "request_oauth_on_install": true,
        "public": false,
        "default_permissions": permissions,
    })
}

/// Lightweight URL host parser: strip scheme, take up to first `/` or `:`.
fn url_host(s: &str) -> Option<String> {
    let after_scheme = s.split("://").nth(1).unwrap_or(s);
    let host_with_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = host_with_port.split(':').next().unwrap_or(host_with_port);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Escape characters that are unsafe inside HTML attribute values.
fn html_attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Manifest conversion response from `POST /app-manifests/{code}/conversions`.
#[derive(Deserialize, Clone)]
struct ManifestConversion {
    id: u64,
    slug: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    webhook_secret: Option<String>,
    pem: String,
}

/// Exchange a manifest code for App credentials via the GitHub API.
///
/// In test builds, the result can be overridden via `EXCHANGE_MANIFEST_RESULT_OVERRIDE`.
async fn exchange_manifest_code(code: &str) -> Result<ManifestConversion, String> {
    // Test override: allow tests to simulate success or failure without
    // making a real HTTP call to the GitHub API.
    #[cfg(test)]
    if let Some(override_result) = EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap().clone() {
        return override_result;
    }

    let url = format!("https://api.github.com/app-manifests/{code}/conversions");
    let client = Client::new();
    let resp = client
        .post(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "djinn-server")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("manifest conversion HTTP {status}: {body}"));
    }
    resp.json::<ManifestConversion>()
        .await
        .map_err(|e| format!("manifest conversion decode: {e}"))
}

/// Test-only override for `exchange_manifest_code`. When set to `Some(...)`,
/// the exchange function returns the overridden result instead of calling the
/// GitHub API. This allows tests to simulate exchange success/failure without
/// network access.
#[cfg(test)]
static EXCHANGE_MANIFEST_RESULT_OVERRIDE: std::sync::Mutex<
    Option<Result<ManifestConversion, String>>,
> = std::sync::Mutex::new(None);

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Register a known setup session token in `state` and return it.
    /// Callers can then use this value as the `djinn_setup_session` cookie
    /// so that `extract_setup_session` validation succeeds.
    async fn register_valid_session(state: &AppState) -> String {
        let token = "test-session-token-42".to_string();
        state
            .set_setup_session_token_for_tests(Some(token.clone()))
            .await;
        token
    }

    /// Build headers with a valid `djinn_setup_session` cookie for the given
    /// session token value.
    fn headers_with_session(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SETUP_SESSION_COOKIE}={token}")).unwrap(),
        );
        headers
    }

    #[test]
    fn extract_cookie_handles_multiple_pairs() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("foo=1; djinn_session=abc; bar=2"),
        );
        assert_eq!(
            extract_cookie(&headers, SESSION_COOKIE),
            Some("abc".to_string())
        );
        assert_eq!(extract_cookie(&headers, "missing"), None);
    }

    #[test]
    fn sanitize_redirect_rejects_external_urls() {
        assert_eq!(sanitize_redirect(Some("/tasks")), "/tasks");
        assert_eq!(sanitize_redirect(Some("https://evil")), "/");
        assert_eq!(sanitize_redirect(Some("//evil")), "/");
        assert_eq!(sanitize_redirect(None), "/");
    }

    #[test]
    fn urlencode_escapes_reserved_chars() {
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
        assert_eq!(
            urlencode("read:user user:email repo"),
            "read%3Auser%20user%3Aemail%20repo"
        );
    }

    #[test]
    fn random_token_is_base64_no_pad_and_32_bytes_of_entropy() {
        let tok = random_token_b64();
        // 32 bytes → 43 base64 chars (url-safe, no padding).
        assert_eq!(tok.len(), 43);
        assert!(!tok.contains('='));
    }

    #[test]
    fn constant_time_eq_matches_std_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn session_expired_rejects_past_timestamps() {
        assert!(session_expired("2000-01-01T00:00:00Z"));
        assert!(!session_expired("2099-01-01T00:00:00Z"));
        assert!(session_expired("not-a-date"));
    }

    #[test]
    fn csrf_state_round_trip_via_constant_time_eq() {
        let token = random_token_b64();
        assert!(constant_time_eq(token.as_bytes(), token.as_bytes()));
        let mut tampered = token.clone().into_bytes();
        tampered[0] ^= 1;
        assert!(!constant_time_eq(token.as_bytes(), &tampered));
    }

    /// `/setup/status` must be reachable without a session and must report
    /// `needs_app_install=true` on a fresh deployment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_status_reports_unconfigured_on_fresh_state() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        let resp = setup_status(State(state)).await;
        let body = resp.0;
        assert!(body.needs_app_install);
        assert!(!body.app_credentials_configured);
        assert!(body.org_login.is_none());
    }

    /// `/setup/status` flips to `needs_app_install=false` only when BOTH the
    /// App config and the org_config row are present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_status_reports_configured_when_both_present() {
        use crate::test_helpers;
        use djinn_db::{NewOrgConfig, OrgConfigRepository};
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

        OrgConfigRepository::new(state.db().clone())
            .set(NewOrgConfig {
                github_org_id: 777,
                github_org_login: "acme",
                app_id: 1,
                installation_id: 42,
            })
            .await
            .unwrap();

        let resp = setup_status(State(state)).await;
        assert!(!resp.0.needs_app_install);
        assert!(resp.0.app_credentials_configured);
        assert_eq!(resp.0.org_login.as_deref(), Some("acme"));
    }

    /// Only one of the two present → still "needs install" but
    /// `app_credentials_configured=true` so the UI shows the picker rather
    /// than the operator runbook screen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_status_half_configured_still_needs_install() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        // Only app_config; no org_config row.
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

        let resp = setup_status(State(state)).await;
        assert!(resp.0.needs_app_install);
        assert!(resp.0.app_credentials_configured);
        assert!(resp.0.org_login.is_none());
    }

    // ─── Self-setup gate tests ───────────────────────────────────────────────

    /// When the override is disabled, `self_setup_enabled()` returns false.
    #[tokio::test]
    async fn self_setup_disabled_by_default() {
        let _lock = with_self_setup_override(Some(false)).await;
        assert!(!self_setup_enabled());
    }

    /// Gate logic: enabled + no credentials → available.
    #[test]
    fn setup_gate_logic_enabled_no_credentials() {
        assert!(self_setup_enabled_or_available(true, false));
    }

    /// Gate logic: enabled + credentials → not available.
    #[test]
    fn setup_gate_logic_enabled_with_credentials() {
        assert!(!self_setup_enabled_or_available(true, true));
    }

    /// Gate logic: disabled → never available.
    #[test]
    fn setup_gate_logic_disabled() {
        assert!(!self_setup_enabled_or_available(false, false));
        assert!(!self_setup_enabled_or_available(false, true));
    }

    /// Internal test helper matching `setup_available` semantics.
    fn self_setup_enabled_or_available(enabled: bool, has_usable: bool) -> bool {
        enabled && !has_usable
    }

    /// `/auth/config` reports `self_setup_available=false` with the gate disabled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_reports_no_self_setup_when_gate_disabled() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(false)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let resp = config(State(state)).await;
        let body = resp.0;
        assert!(!body.self_setup_available);
    }

    /// `/auth/config` reports `self_setup_available=true` when the gate
    /// is enabled and no usable credentials exist.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_reports_self_setup_when_enabled_and_unconfigured() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let resp = config(State(state)).await;
        let body = resp.0;
        assert!(body.self_setup_available);
    }

    /// `/auth/config` reports `self_setup_available=false` when the gate
    /// is enabled but usable credentials already exist.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_reports_no_self_setup_when_credentials_present() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
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

        let resp = config(State(state)).await;
        let body = resp.0;
        assert!(!body.self_setup_available);
    }

    // ─── Setup route gate tests ──────────────────────────────────────────────

    /// When the gate is disabled, `create_app` returns 404.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_app_returns_404_when_gate_disabled() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(false)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let headers = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// When credentials exist, `create_app` returns 404 even with gate enabled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_app_returns_404_when_credentials_present() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
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

        let headers = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// `app_manifest_callback` returns 404 when gate is disabled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_returns_404_when_gate_disabled() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(false)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let headers = HeaderMap::new();
        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: None,
                state: None,
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ─── Boot token exchange tests ───────────────────────────────────────────

    /// Valid boot token exchange: consumes token, sets setup session cookie,
    /// returns 303 to clean `/auth/github/create-app`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn valid_boot_token_exchange_sets_session_and_redirects() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let (raw_token, bt) = boot_token::BootToken::generate();
        state.set_boot_token_for_tests(Some(bt)).await;

        let headers = HeaderMap::new();
        let resp = create_app(
            State(state.clone()),
            Query(CreateAppQuery {
                setup_token: Some(raw_token.clone()),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "should be 303");

        // Verify redirect target is the clean URL (no token leaked).
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(location, "/auth/github/create-app");
        assert!(
            !location.contains("setup_token"),
            "token must not leak in Location"
        );

        // Verify the setup session cookie was set.
        let set_cookies: Vec<_> = resp.headers().get_all(header::SET_COOKIE).iter().collect();
        let setup_cookie = set_cookies
            .iter()
            .find(|c| c.to_str().unwrap_or("").contains("djinn_setup_session="))
            .expect("setup session cookie must be set");
        let cookie_str = setup_cookie.to_str().unwrap();
        assert!(cookie_str.contains("HttpOnly"), "cookie must be HttpOnly");
        assert!(
            cookie_str.contains("SameSite=Lax"),
            "cookie must be SameSite=Lax"
        );
        assert!(
            cookie_str.contains("Path=/auth/github"),
            "cookie must be path-scoped"
        );
        assert!(
            cookie_str.contains("Max-Age=900"),
            "cookie must expire in 15 min (900s)"
        );
    }

    /// An invalid/unknown token is rejected with 403.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_boot_token_is_rejected() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let (_raw, bt) = boot_token::BootToken::generate();
        state.set_boot_token_for_tests(Some(bt)).await;

        let headers = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery {
                setup_token: Some("not-a-valid-token".into()),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// An already-used token is rejected (single-use behavior).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn used_boot_token_is_rejected() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let (raw_token, bt) = boot_token::BootToken::generate();
        state.set_boot_token_for_tests(Some(bt)).await;

        // First exchange succeeds.
        let headers = HeaderMap::new();
        let resp1 = create_app(
            State(state.clone()),
            Query(CreateAppQuery {
                setup_token: Some(raw_token.clone()),
            }),
            headers,
        )
        .await;
        assert_eq!(resp1.status(), StatusCode::SEE_OTHER);

        // Second exchange with the same token is rejected.
        let headers = HeaderMap::new();
        let resp2 = create_app(
            State(state),
            Query(CreateAppQuery {
                setup_token: Some(raw_token),
            }),
            headers,
        )
        .await;
        assert_eq!(resp2.status(), StatusCode::FORBIDDEN);
    }

    /// When no boot token has been generated, exchange returns 410 Gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_boot_token_returns_410() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let headers = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery {
                setup_token: Some("anything".into()),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::GONE);
    }

    /// Empty `setup_token` query is rejected with 400.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_boot_token_returns_400() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let headers = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery {
                setup_token: Some(String::new()),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Without a token and without a setup session, create_app returns 401.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_app_without_token_or_session_returns_401() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let headers = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// With a valid setup session cookie, create_app proceeds (200 placeholder).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_app_with_valid_session_returns_200() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;

        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers_with_session(&session),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `app_manifest_callback` with a valid setup session but no code/state
    /// returns 400 (missing code or state).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_with_valid_session_but_no_code_returns_400() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;

        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: None,
                state: None,
            }),
            headers_with_session(&session),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `app_manifest_callback` without a session returns 401.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_without_session_returns_401() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let headers = HeaderMap::new();
        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: None,
                state: None,
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Token exchange response never leaks the raw token in the Location header.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn token_exchange_clean_redirect_no_leak() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let (raw_token, bt) = boot_token::BootToken::generate();
        state.set_boot_token_for_tests(Some(bt)).await;

        let headers = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery {
                setup_token: Some(raw_token),
            }),
            headers,
        )
        .await;

        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(location, "/auth/github/create-app");
        assert!(
            !location.contains('?'),
            "Location must not contain query params"
        );
    }

    /// No loopback/IP/proxy-header bypass: setup routes are gated
    /// purely by the env var + credential state, not by request headers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_loopback_bypass_on_setup_routes() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(false)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("127.0.0.1"));
        headers.insert("x-real-ip", HeaderValue::from_static("127.0.0.1"));

        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ─── Manifest JSON shape tests ──────────────────────────────────────────

    #[test]
    fn manifest_json_has_expected_shape() {
        let manifest = build_manifest_json("https://djinn.example.com");
        // App name prefix is `djinn-`
        assert_eq!(manifest["name"], "djinn-djinn.example.com");
        assert_eq!(manifest["url"], "https://djinn.example.com");
        assert_eq!(
            manifest["redirect_url"],
            "https://djinn.example.com/auth/github/app-manifest-callback"
        );
        assert_eq!(
            manifest["callback_urls"][0],
            "https://djinn.example.com/auth/github/callback"
        );
        assert_eq!(
            manifest["hook_attributes"]["url"],
            "https://djinn.example.com/webhooks/github"
        );
        assert_eq!(manifest["hook_attributes"]["active"], false);
        assert_eq!(manifest["request_oauth_on_install"], true);
        assert_eq!(manifest["public"], false);
        // Permissions: exactly contents:write, pull_requests:write
        // (metadata:read is granted by GitHub automatically, not listed).
        assert_eq!(manifest["default_permissions"]["contents"], "write");
        assert_eq!(manifest["default_permissions"]["pull_requests"], "write");
        assert!(manifest["default_permissions"].get("metadata").is_none());
        // No extra permissions beyond the required set.
        let perms = manifest["default_permissions"].as_object().unwrap();
        assert_eq!(perms.len(), 2, "only contents, pull_requests");
        // Round-trips as valid JSON.
        let s = manifest.to_string();
        let _back: serde_json::Value = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn manifest_json_name_uses_djinn_prefix() {
        let manifest = build_manifest_json("https://mycompany.example.com");
        let name = manifest["name"].as_str().unwrap();
        assert!(
            name.starts_with("djinn-"),
            "name must start with djinn-, got: {name}"
        );
    }

    #[test]
    fn manifest_json_no_extra_events() {
        let manifest = build_manifest_json("https://djinn.example.com");
        // No default_events field — the design requires only permissions.
        assert!(
            manifest.get("default_events").is_none(),
            "manifest should not include default_events"
        );
    }

    #[test]
    fn manifest_url_host_handles_localhost_fallback() {
        assert_eq!(
            url_host("http://127.0.0.1:8372").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            url_host("https://djinn.example.com/path").as_deref(),
            Some("djinn.example.com")
        );
        assert_eq!(url_host("not a url").as_deref(), Some("not a url"));
    }

    #[test]
    fn html_attr_escape_neutralises_quotes_and_brackets() {
        let raw = "<\"&'>";
        assert_eq!(html_attr_escape(raw), "&lt;&quot;&amp;&#39;&gt;");
    }

    // ─── Manifest state mismatch tests ──────────────────────────────────────

    /// `app_manifest_callback` with a valid setup session but missing manifest
    /// state cookie returns 400.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_missing_state_cookie_returns_400() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;

        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: Some("some-code".into()),
                state: Some("some-state".into()),
            }),
            headers_with_session(&session),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `app_manifest_callback` with mismatched manifest state cookie returns 400.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_state_mismatch_returns_400() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{}=cookie-state", MANIFEST_STATE_COOKIE)).unwrap(),
        );

        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: Some("some-code".into()),
                state: Some("different-state".into()),
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ─── Setup gate + credential presence tests ─────────────────────────────

    /// After credentials are persisted, `create_app` returns 404 because
    /// the gate detects usable credentials.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_app_returns_404_after_credentials_persisted() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;

        // Initially setup is available (no credentials).
        let headers = headers_with_session(&session);
        let resp = create_app(
            State(state.clone()),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "should proceed before credentials"
        );

        // Simulate credential persistence (hot-reload).
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

        // Now setup should return 404.
        let headers2 = HeaderMap::new();
        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers2,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "should be 404 after credentials"
        );
    }

    /// After credentials are persisted, `app_manifest_callback` returns 404.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_returns_404_after_credentials_persisted() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
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

        let headers = HeaderMap::new();
        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: Some("code".into()),
                state: Some("state".into()),
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ─── Invalid setup session rejection ──────────────────────────────────

    /// An arbitrary (non-registered) setup session cookie is rejected even
    /// when self-setup is enabled and no credentials exist.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_app_rejects_invalid_setup_session() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        // Store a *different* token in state so validation fails.
        state
            .set_setup_session_token_for_tests(Some("the-real-token".into()))
            .await;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SETUP_SESSION_COOKIE}=forged-token")).unwrap(),
        );

        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "arbitrary session cookie must be rejected"
        );
    }

    /// `app_manifest_callback` rejects an invalid setup session cookie even
    /// when a valid manifest state cookie and code/state pair are present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_rejects_invalid_setup_session() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_setup_session_token_for_tests(Some("the-real-token".into()))
            .await;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!(
                "{SETUP_SESSION_COOKIE}=forged-token; {MANIFEST_STATE_COOKIE}=csrf"
            ))
            .unwrap(),
        );

        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: Some("code".into()),
                state: Some("csrf".into()),
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ─── Exchange failure preserves retry ability ─────────────────────────

    /// When `exchange_manifest_code` fails after a valid CSRF state check,
    /// the handler returns 502, clears the manifest state cookie, but
    /// preserves the setup session cookie so the user can retry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_exchange_failure_preserves_setup_session() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() =
            Some(Err("simulated GitHub API failure".into()));

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        let csrf = "test-csrf-token".to_string();

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("expired-code".into()),
                state: Some(csrf),
            }),
            headers,
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::BAD_GATEWAY,
            "exchange failure should return 502"
        );

        // Manifest state cookie should be cleared (allows re-initiating the flow).
        // Setup session cookie should NOT be cleared (allows retry).
        let set_cookies: Vec<_> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap_or("").to_string())
            .collect();
        assert!(
            set_cookies
                .iter()
                .any(|c| c.starts_with(MANIFEST_STATE_COOKIE) && c.contains("Max-Age=0")),
            "manifest state cookie must be cleared on exchange failure: {set_cookies:?}"
        );
        // The setup session cookie should NOT appear — it was not cleared.
        assert!(
            !set_cookies
                .iter()
                .any(|c| c.starts_with(SETUP_SESSION_COOKIE) && c.contains("Max-Age=0")),
            "setup session cookie must NOT be cleared on exchange failure: {set_cookies:?}"
        );

        // Clean up the override.
        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    // ─── Successful persistence/hot-reload + cookie clearing ──────────────

    /// On a successful manifest exchange the callback persists the returned
    /// credentials, hot-reloads the active App config, clears both the
    /// manifest state and setup session cookies, and redirects to the new
    /// App's install URL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_success_persists_hot_reloads_and_clears_cookies() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        // Bypass real DB persistence — this test exercises the full callback
        // flow (exchange → build AppConfig → persist → hot-reload → cookies)
        // without requiring a running Postgres instance.
        state.set_test_bypass_persist(true).await;
        let csrf = "test-csrf-token".to_string();

        // Inject a mock exchange result.
        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "test-secret".into(),
            webhook_secret: Some("test-webhook-secret".into()),
            pem: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
        }));

        // Before the callback, no credentials should exist.
        assert!(
            state.app_config().await.is_none(),
            "no credentials before callback"
        );

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("valid-manifest-code".into()),
                state: Some(csrf),
            }),
            headers,
        )
        .await;

        // Must redirect to the install URL.
        assert_eq!(
            resp.status(),
            StatusCode::FOUND,
            "successful callback must redirect"
        );
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            location.contains("github.com/apps/djinn-test/installations/new"),
            "must redirect to the new App install URL, got: {location}"
        );
        // No setup token or session nonce leaks into the redirect.
        assert!(
            !location.contains("setup_token"),
            "setup_token must not appear in Location: {location}"
        );

        // Both cookies must be cleared.
        let set_cookies: Vec<_> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap_or("").to_string())
            .collect();
        assert!(
            set_cookies
                .iter()
                .any(|c| c.starts_with(MANIFEST_STATE_COOKIE) && c.contains("Max-Age=0")),
            "manifest state cookie must be cleared after success: {set_cookies:?}"
        );
        assert!(
            set_cookies
                .iter()
                .any(|c| c.starts_with(SETUP_SESSION_COOKIE) && c.contains("Max-Age=0")),
            "setup session cookie must be cleared after success: {set_cookies:?}"
        );

        // Credentials must have been persisted and hot-reloaded.
        let cfg = state.app_config().await;
        assert!(
            cfg.is_some(),
            "credentials must be present after persistence"
        );
        let cfg = cfg.unwrap();
        assert_eq!(cfg.app_id, 42);
        assert_eq!(cfg.slug, "djinn-test");
        assert_eq!(cfg.client_id, "Iv1.test-client");

        // After credentials are loaded, setup routes must return 404.
        let resp = create_app(
            State(state.clone()),
            Query(CreateAppQuery { setup_token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "setup routes must be 404 after credentials persisted"
        );

        // Setup session is also invalidated — the stored token was cleared
        // after persistence, so the old cookie no longer validates.
        let headers = headers_with_session(&session);
        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "routes are 404 due to credentials, regardless of session"
        );

        // Clean up the override.
        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }
}
