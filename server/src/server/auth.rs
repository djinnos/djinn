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

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use reqwest::Client;
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use djinn_db::{
    CreateUserAuthSession, NewOrgConfig, OrgConfigRepository, SessionAuthRepository,
    UserAuthSessionRecord, UserRepository,
};
use djinn_provider::github_app::jwt::mint_app_jwt_anyhow;

const SESSION_COOKIE: &str = "djinn_session";
const OAUTH_STATE_COOKIE: &str = "djinn_oauth_state";
const DEFAULT_PUBLIC_URL: &str = "http://127.0.0.1:8372";
const SESSION_TTL_SECS: i64 = 60 * 60 * 24 * 30; // 30 days
const STATE_COOKIE_TTL_SECS: i64 = 60 * 10; // 10 minutes

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
        .route(
            "/auth/github/app-setup-callback",
            get(app_setup_callback),
        )
        .route("/auth/logout", post(logout))
        .route("/setup/status", get(setup_status))
}

#[derive(Serialize)]
struct ConfigResponse {
    configured: bool,
    missing: Vec<&'static str>,
    setup_doc_url: &'static str,
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

    Json(ConfigResponse {
        configured: active.is_some(),
        missing,
        setup_doc_url: "https://github.com/djinnos/djinn/blob/main/docs/GITHUB_APP_SETUP.md",
    })
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
    /// The raw cookie token, for callers that want to refresh or revoke it.
    #[serde(skip)]
    pub session_token: String,
    /// The GitHub user access token, used to call user-scoped GitHub APIs
    /// (e.g. `GET /user/installations`). Never serialised to clients.
    #[serde(skip)]
    pub github_access_token: String,
}

impl From<UserAuthSessionRecord> for AuthenticatedUser {
    fn from(row: UserAuthSessionRecord) -> Self {
        Self {
            id: row.user_id,
            login: row.github_login,
            name: row.github_name,
            avatar_url: row.github_avatar_url,
            session_token: row.token,
            github_access_token: row.github_access_token,
        }
    }
}

/// Resolve a request's `djinn_session` cookie into an [`AuthenticatedUser`],
/// if any. Returns `Ok(None)` for the unauthenticated case; returns `Err`
/// only on database errors.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
) -> djinn_db::Result<Option<AuthenticatedUser>> {
    let Some(token) = extract_cookie(headers, SESSION_COOKIE) else {
        return Ok(None);
    };
    let repo = SessionAuthRepository::new(state.db().clone());
    let Some(row) = repo.get_by_token(&token).await? else {
        return Ok(None);
    };
    if session_expired(&row.expires_at) {
        // Best-effort cleanup; ignore errors.
        let _ = repo.delete_by_token(&token).await;
        return Ok(None);
    }
    Ok(Some(row.into()))
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct MeResponse {
    id: String,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    /// GitHub org this deployment is locked to. Surfaced so the web client can
    /// show "signed in as <login> on <org>" without a second round-trip.
    /// `None` when the deployment hasn't finished the manifest flow yet.
    org_login: Option<String>,
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Phase 2: prefer the session→users join so we surface the stable
    // `users.id` surrogate and pick up any renamed login/avatar from GitHub
    // the next time `upsert_from_github` ran.
    let Some(token) = extract_cookie(&headers, SESSION_COOKIE) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let sessions = SessionAuthRepository::new(state.db().clone());

    // Try the join path first. If the row has no `user_fk` (legacy Phase 1
    // session), fall back to the flat fetch so we don't break already-signed-in
    // browsers during the rollout.
    match sessions.get_by_token_with_user(&token).await {
        Ok(Some((session, user))) => {
            if session_expired(&session.expires_at) {
                let _ = sessions.delete_by_token(&token).await;
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let org_login = org_login_for_response(&state).await;
            return Json(MeResponse {
                id: user.id,
                login: user.github_login,
                name: user.github_name,
                avatar_url: user.github_avatar_url,
                org_login,
            })
            .into_response();
        }
        Ok(None) => {
            // Either unknown token or legacy (user_fk IS NULL). Fall through.
        }
        Err(e) => {
            tracing::error!(error = %e, "auth /me: db error on joined fetch");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    match authenticate(&state, &headers).await {
        Ok(Some(user)) => {
            let org_login = org_login_for_response(&state).await;
            Json(MeResponse {
                id: user.id,
                login: user.login,
                name: user.name,
                avatar_url: user.avatar_url,
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

    // 1. Exchange code for access token.
    let callback_url = format!("{}/auth/github/callback", public_url());
    let access_token = match exchange_code(&client_id, &client_secret, &code, &callback_url).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "auth callback: token exchange failed");
            return (StatusCode::BAD_GATEWAY, "token exchange failed").into_response();
        }
    };

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

    // 6. Persist a new session row, linked to the users table via `user_fk`.
    let token = random_token_b64();
    let expires_at = rfc3339_in(SESSION_TTL_SECS);
    let repo = SessionAuthRepository::new(state.db().clone());
    if let Err(e) = repo
        .create_with_user_fk(
            CreateUserAuthSession {
                token: &token,
                user_id: &user.id.to_string(),
                github_login: &user.login,
                github_name: user.name.as_deref(),
                github_avatar_url: user.avatar_url.as_deref(),
                github_access_token: &access_token,
                expires_at: &expires_at,
            },
            &user_row.id,
        )
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
    if let Some(token) = extract_cookie(&headers, SESSION_COOKIE) {
        let repo = SessionAuthRepository::new(state.db().clone());
        if let Err(e) = repo.delete_by_token(&token).await {
            tracing::warn!(error = %e, "auth /logout: failed to delete session row");
        }
    }
    let mut resp_headers = HeaderMap::new();
    clear_cookie(&mut resp_headers, SESSION_COOKIE);
    (StatusCode::NO_CONTENT, resp_headers).into_response()
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

async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<String, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        client_id: &'a str,
        client_secret: &'a str,
        code: &'a str,
        redirect_uri: &'a str,
    }
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        error_description: Option<String>,
    }

    let client = Client::new();
    let resp: Resp = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .json(&Req {
            client_id,
            client_secret,
            code,
            redirect_uri,
        })
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    if let Some(err) = resp.error {
        return Err(format!(
            "{err}: {}",
            resp.error_description.unwrap_or_default()
        ));
    }
    resp.access_token
        .ok_or_else(|| "missing access_token in response".to_string())
}

async fn fetch_github_user(access_token: &str) -> Result<GhUser, String> {
    let client = Client::new();
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "djinn-server")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GitHub /user returned {status}: {body}"));
    }
    resp.json::<GhUser>().await.map_err(|e| e.to_string())
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
    #[derive(Deserialize)]
    struct Membership {
        #[serde(default)]
        state: Option<String>,
    }

    // Percent-encode the org segment so a weird login can't escape the path.
    let url = format!(
        "https://api.github.com/user/memberships/orgs/{}",
        urlencode(org_login),
    );
    let client = Client::new();
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "djinn-server")
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN {
        return Ok(false);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GitHub /user/memberships/orgs/{org_login} returned {status}: {body}"
        ));
    }
    let parsed: Membership = resp.json().await.map_err(|e| e.to_string())?;
    Ok(parsed.state.as_deref() == Some("active"))
}

// ─── Cookie + misc helpers ────────────────────────────────────────────────────

fn public_url() -> String {
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

fn set_cookie(headers: &mut HeaderMap, name: &str, value: &str, max_age: i64) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie =
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}");
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

fn clear_cookie(headers: &mut HeaderMap, name: &str) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}");
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

fn extract_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
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

fn random_token_b64() -> String {
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .expect("SystemRandom available");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn rfc3339_in(seconds: i64) -> String {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds);
    t.format(&Rfc3339).unwrap_or_else(|_| String::new())
}

fn session_expired(expires_at: &str) -> bool {
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

fn urlencode(s: &str) -> String {
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
                format!(
                    "Failed to fetch installation {installation_id} from GitHub: {e}"
                ),
            )
                .into_response();
        }
    };

    if !installation.account.account_type.eq_ignore_ascii_case("Organization") {
        tracing::warn!(
            installation_id,
            account_type = %installation.account.account_type,
            account_login = %installation.account.login,
            "app_setup_callback: rejecting non-org installation",
        );
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "This deployment requires a GitHub *organization* installation, \
                 but installation {installation_id} is bound to account '{}' (type={}). \
                 Reinstall the App on an organization.",
                installation.account.login, installation.account.account_type,
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
        && existing.github_org_id as u64 == installation.account.id
    {
        tracing::info!(
            installation_id,
            action,
            "app_setup_callback: re-entry for already-bound org, redirecting home",
        );
        return redirect_to_web();
    }

    if let Err(e) = org_repo
        .set_or_replace(NewOrgConfig {
            github_org_id: installation.account.id as i64,
            github_org_login: &installation.account.login,
            app_id: cfg.app_id as i64,
            installation_id: installation_id as i64,
        })
        .await
    {
        tracing::error!(
            error = %e,
            installation_id,
            account = %installation.account.login,
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
        account = %installation.account.login,
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

/// Fetch an installation's account info via the App JWT. Returns only the
/// fields `app_setup_callback` needs for the `org_config` binding (numeric
/// account id, login, and account type).
async fn fetch_installation_for_setup(installation_id: u64) -> Result<InstallationDetail, String> {
    let jwt = mint_app_jwt_anyhow().map_err(|e| e.to_string())?;
    let url = format!("https://api.github.com/app/installations/{installation_id}");
    let client = Client::new();
    let resp = client
        .get(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "djinn-server")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("{status}: {body}"));
    }
    resp.json::<InstallationDetail>()
        .await
        .map_err(|e| format!("decode /app/installations/{installation_id}: {e}"))
}

#[derive(Deserialize)]
struct InstallationDetail {
    #[allow(dead_code)]
    id: u64,
    account: InstallationAccount,
}

#[derive(Deserialize)]
struct InstallationAccount {
    id: u64,
    #[serde(default)]
    login: String,
    #[serde(rename = "type", default)]
    account_type: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
            .set_or_replace(NewOrgConfig {
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
}
