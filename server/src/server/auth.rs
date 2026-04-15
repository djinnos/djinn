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
//!   * `GITHUB_APP_CLIENT_ID`       — GitHub App client id (required).
//!   * `GITHUB_APP_CLIENT_SECRET`   — GitHub App client secret (required).
//!   * `GITHUB_APP_SLUG`            — App slug, used when `?install=1` is
//!                                    passed to redirect to the install
//!                                    page post-auth.
//!   * `DJINN_PUBLIC_URL`           — Public base URL used to build the
//!                                    OAuth callback (defaults to
//!                                    `http://127.0.0.1:8372`).
//!   * `DJINN_COOKIE_SECURE`        — `true` to force `Secure` on the
//!                                    session cookie.
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
use djinn_db::{CreateUserAuthSession, SessionAuthRepository, UserAuthSessionRecord};

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
        .route("/auth/github/start", get(github_start))
        .route("/auth/github/callback", get(github_callback))
        .route("/auth/logout", post(logout))
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
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authenticate(&state, &headers).await {
        Ok(Some(user)) => Json(MeResponse {
            id: user.id,
            login: user.login,
            name: user.name,
            avatar_url: user.avatar_url,
        })
        .into_response(),
        Ok(None) => StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "auth /me: db error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
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

async fn github_start(Query(q): Query<StartQuery>) -> Response {
    let client_id = match read_github_app_oauth_env("GITHUB_APP_CLIENT_ID") {
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
        HeaderValue::from_str(&auth_url)
            .unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    (StatusCode::FOUND, headers).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn github_callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Response {
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

    let client_id = read_github_app_oauth_env("GITHUB_APP_CLIENT_ID").unwrap_or_default();
    let client_secret =
        read_github_app_oauth_env("GITHUB_APP_CLIENT_SECRET").unwrap_or_default();
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

    // 3. Persist a new session row.
    let token = random_token_b64();
    let expires_at = rfc3339_in(SESSION_TTL_SECS);
    let repo = SessionAuthRepository::new(state.db().clone());
    if let Err(e) = repo
        .create(CreateUserAuthSession {
            token: &token,
            user_id: &user.id.to_string(),
            github_login: &user.login,
            github_name: user.name.as_deref(),
            github_avatar_url: user.avatar_url.as_deref(),
            github_access_token: &access_token,
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
    let location = if want_install {
        match std::env::var("GITHUB_APP_SLUG") {
            Ok(slug) if !slug.trim().is_empty() => {
                format!(
                    "https://github.com/apps/{}/installations/new",
                    slug.trim()
                )
            }
            _ => {
                tracing::warn!(
                    "auth callback: install=1 requested but GITHUB_APP_SLUG is unset"
                );
                sanitize_redirect(Some(&redirect))
            }
        }
    } else {
        sanitize_redirect(Some(&redirect))
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

// ─── Cookie + misc helpers ────────────────────────────────────────────────────

fn public_url() -> String {
    std::env::var("DJINN_PUBLIC_URL").unwrap_or_else(|_| DEFAULT_PUBLIC_URL.to_string())
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
    let cookie = format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

fn clear_cookie(headers: &mut HeaderMap, name: &str) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie =
        format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}");
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(urlencode("read:user user:email repo"),
            "read%3Auser%20user%3Aemail%20repo");
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
}
