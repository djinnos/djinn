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
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
use djinn_db::{
    CreateUserAuthSession, NewOrgConfig, OrgConfig, OrgConfigRepository, SessionAuthRepository,
    UserRepository,
};
use djinn_provider::github_app::jwt::mint_app_jwt_anyhow;
use djinn_provider::github_app::{CredentialSourceState, ManifestConversion};
use djinn_provider::github_server::{AppInstallation, GitHubServerClient};
use djinn_provider::oauth::github_app_user::{self, GithubUserTokens};

pub(super) const SESSION_COOKIE: &str = "djinn_session";
const OAUTH_STATE_COOKIE: &str = "djinn_oauth_state";
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
/// Browser-only capability cookie that gates the explicit setup CTA.
const SETUP_LAUNCH_COOKIE: &str = "djinn_setup_launch";
/// Keep the launch capability off every other auth route, including the
/// manifest callback that receives a cross-site navigation from GitHub.
const SETUP_LAUNCH_PATH: &str = "/auth/github/setup-start";
/// The setup CTA should be used promptly. The server independently enforces
/// the same lifetime; this is not merely a browser cookie hint.
const SETUP_LAUNCH_TTL_SECS: i64 = 60 * 2;
/// Query parameter name for the install-continuation nonce appended to the
/// GitHub install URL after manifest credential persistence.
const INSTALL_CONTINUATION_PARAM: &str = "djinn_continuation";
/// Cookie name for the install-continuation nonce. This cookie carries the
/// nonce through the cross-domain GitHub install round-trip (manifest
/// callback → GitHub install page → `/auth/github/callback` →
/// `/auth/github/app-setup-callback`) because GitHub does not echo custom
/// query parameters on its redirects.
const INSTALL_CONTINUATION_COOKIE: &str = "djinn_install_continuation";
/// TTL for the install-continuation cookie. Matches the realistic window for
/// the user to complete the GitHub install after credentials are persisted.
const INSTALL_CONTINUATION_TTL_SECS: i64 = 60 * 10; // 10 minutes

/// Read a GitHub App OAuth client id/secret from the environment.
///
/// The legacy `GITHUB_OAUTH_CLIENT_ID` / `GITHUB_OAUTH_CLIENT_SECRET`
/// fallbacks were retired with the GitHub App finalization — only the
/// App-native env var names are honoured going forward.
fn read_github_app_oauth_env(primary: &str) -> Option<String> {
    std::env::var(primary).ok().filter(|v| !v.is_empty())
}

/// Opt-in for GitHub App installations owned by a personal account. Default
/// remains organization-only for production deployments.
pub(super) fn allow_user_installations() -> bool {
    parse_allow_user_installations(
        std::env::var("DJINN_ALLOW_USER_INSTALLATIONS")
            .ok()
            .as_deref(),
    )
}

fn parse_allow_user_installations(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        let value = value.trim();
        value == "1" || value.eq_ignore_ascii_case("true")
    })
}

/// Account types accepted for deployment binding. Unknown GitHub account
/// types remain rejected even when personal-account support is enabled.
pub(super) fn installation_account_type_allowed(account_type: &str, allow_users: bool) -> bool {
    account_type.eq_ignore_ascii_case("Organization")
        || (allow_users && account_type.eq_ignore_ascii_case("User"))
}

/// Personal bindings reuse the legacy `org_config` row, so identify them by
/// matching the immutable GitHub account id plus login to the signed-in user.
/// GitHub account ids are global across users and organizations.
pub(super) fn binding_matches_user(
    binding: &OrgConfig,
    github_user_id: i64,
    github_login: &str,
) -> bool {
    binding.github_org_id == github_user_id
        && binding.github_org_login.eq_ignore_ascii_case(github_login)
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
        .route("/auth/github/setup-start", post(setup_start))
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
    /// Whether this specific browser request can use the local-only setup
    /// CTA. This is stricter than `self_setup_available`, which continues to
    /// describe the boot-token fallback for any deployment.
    #[serde(default)]
    setup_launch_available: bool,
    credential_source: Option<&'static str>,
    setup_state: &'static str,
    setup_error: Option<String>,
    setup_retryable: bool,
    credentials_unrecoverable: bool,
}

/// Keep the JSON response shape unchanged while attaching a browser-only
/// launch cookie when this was a verified same-origin config fetch.
struct ConfigOutput(ConfigResponse, HeaderMap);

impl IntoResponse for ConfigOutput {
    fn into_response(self) -> Response {
        (self.1, Json(self.0)).into_response()
    }
}

struct CredentialStatusFields {
    credential_source: Option<&'static str>,
    setup_state: &'static str,
    setup_error: Option<String>,
    setup_retryable: bool,
    credentials_unrecoverable: bool,
}

fn credential_status_fields(state: &CredentialSourceState) -> CredentialStatusFields {
    match state {
        CredentialSourceState::ValidSecret(_) => CredentialStatusFields {
            credential_source: Some("secret"),
            setup_state: "valid_secret",
            setup_error: None,
            setup_retryable: false,
            credentials_unrecoverable: false,
        },
        CredentialSourceState::ValidPersisted(_) => CredentialStatusFields {
            credential_source: Some("persisted"),
            setup_state: "valid_persisted",
            setup_error: None,
            setup_retryable: false,
            credentials_unrecoverable: false,
        },
        CredentialSourceState::InvalidSecret(detail) => CredentialStatusFields {
            credential_source: None,
            setup_state: "invalid_secret",
            setup_error: Some(format!(
                "GitHub App Secret is invalid or incomplete: {}",
                detail.issues.join(", ")
            )),
            setup_retryable: false,
            credentials_unrecoverable: false,
        },
        CredentialSourceState::UndecryptablePersisted => CredentialStatusFields {
            credential_source: None,
            setup_state: "credentials_unrecoverable",
            setup_error: Some(
                "Persisted GitHub App credentials cannot be decrypted; restore the vault key, clear the persisted credentials and rerun setup, or mount a valid Secret"
                    .to_string(),
            ),
            setup_retryable: false,
            credentials_unrecoverable: true,
        },
        CredentialSourceState::Unconfigured => CredentialStatusFields {
            credential_source: None,
            setup_state: "unconfigured",
            setup_error: None,
            setup_retryable: false,
            credentials_unrecoverable: false,
        },
    }
}

/// Report whether the GitHub App is configured (env-only after the K8s
/// migration). Used by the UI to decide between sign-in and a static
/// "App not configured" notice.
async fn config(State(state): State<AppState>, headers: HeaderMap) -> ConfigOutput {
    let credential_state = state.app_credential_state().await;
    let active = credential_state.app_config().cloned();
    let status = credential_status_fields(&credential_state);
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

    let self_setup_available = setup_available(&credential_state);
    let setup_launch_available =
        self_setup_available && local_setup_launch_available(&headers, &public_url());

    let mut response_headers = HeaderMap::new();
    if setup_launch_available {
        let launch_token = state
            .issue_setup_launch_capability(std::time::Duration::from_secs(
                SETUP_LAUNCH_TTL_SECS as u64,
            ))
            .await;
        set_setup_launch_cookie(&mut response_headers, &launch_token);
    }

    ConfigOutput(
        ConfigResponse {
            configured: active.is_some(),
            missing,
            setup_doc_url: "https://github.com/djinnos/djinn/blob/main/docs/GITHUB_APP_SETUP.md",
            self_setup_available,
            setup_launch_available,
            credential_source: status.credential_source,
            setup_state: status.setup_state,
            setup_error: status.setup_error,
            setup_retryable: status.setup_retryable,
            credentials_unrecoverable: status.credentials_unrecoverable,
        },
        response_headers,
    )
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

/// Serialises self-setup tests and restores the process-global credential
/// cache when each test finishes. Manifest callback tests intentionally
/// exercise the production hot-reload path, so leaving that cache populated
/// would leak one test's App into unrelated auth tests.
#[cfg(test)]
struct SelfSetupTestGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for SelfSetupTestGuard {
    fn drop(&mut self) {
        SELF_SETUP_OVERRIDE.store(-1, Ordering::SeqCst);
        djinn_provider::github_app::clear_runtime_config();
        if let Ok(mut slot) = EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = OAUTH_EXCHANGE_RESULT_OVERRIDE.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = GITHUB_USER_RESULT_OVERRIDE.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = USER_INSTALLATIONS_RESULT_OVERRIDE.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = APP_INSTALLATION_RESULT_OVERRIDE.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = ORG_CONFIG_RESULT_OVERRIDE.lock() {
            *slot = None;
        }
    }
}

/// Acquire the async test lock, set the override, and return the guard.
/// The override stays set until the guard is dropped (end of test).
#[cfg(test)]
async fn with_self_setup_override(value: Option<bool>) -> SelfSetupTestGuard {
    let guard = SELF_SETUP_TEST_LOCK.lock().await;
    djinn_provider::github_app::clear_runtime_config();
    let v = match value {
        None => -1,
        Some(true) => 1,
        Some(false) => 0,
    };
    SELF_SETUP_OVERRIDE.store(v, Ordering::SeqCst);
    SelfSetupTestGuard { _guard: guard }
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
/// the retained state is truly unconfigured. Invalid Secret and
/// undecryptable-persisted states require explicit recovery and never fall
/// through to setup.
fn setup_available(state: &CredentialSourceState) -> bool {
    self_setup_enabled() && matches!(state, CredentialSourceState::Unconfigured)
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

/// Set the short-lived capability used only by the setup CTA POST.
///
/// `SameSite=Strict` keeps the browser from attaching it to a cross-site
/// form or fetch. The exact path avoids carrying it through the GitHub
/// redirect/callback flow, and HttpOnly keeps UI JavaScript from turning it
/// into another exposed boot token.
fn set_setup_launch_cookie(headers: &mut HeaderMap, value: &str) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie = format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}",
        name = SETUP_LAUNCH_COOKIE,
        path = SETUP_LAUNCH_PATH,
        max_age = SETUP_LAUNCH_TTL_SECS,
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

fn clear_setup_launch_cookie(headers: &mut HeaderMap) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie = format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Strict; Max-Age=0; \
         Expires=Thu, 01 Jan 1970 00:00:00 GMT{secure}",
        name = SETUP_LAUNCH_COOKIE,
        path = SETUP_LAUNCH_PATH,
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

/// Recover a same-origin operator from a stale setup page without exposing
/// why capability validation failed. The SPA reload fetches `/auth/config`,
/// which mints a fresh launch cookie when the local setup gate remains open.
fn setup_launch_expired_redirect() -> Response {
    let mut response_headers = HeaderMap::new();
    clear_setup_launch_cookie(&mut response_headers);
    response_headers.insert(
        header::LOCATION,
        HeaderValue::from_static("/?setup=expired"),
    );
    (StatusCode::SEE_OTHER, response_headers).into_response()
}

fn sec_fetch_site_is_same_origin(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("same-origin"))
}

#[derive(Debug, Clone)]
struct SetupLaunchOrigin {
    scheme: String,
    authority: Authority,
}

/// The browser CTA is deliberately local-only. Fetch metadata and Origin are
/// browser CSRF defenses, not an authorization boundary for arbitrary HTTP
/// clients; restricting the configured callback origin to the OS loopback
/// interface gives the direct-client path an explicit local trust boundary.
fn configured_loopback_setup_origin(public_url: &str) -> Option<SetupLaunchOrigin> {
    let uri = public_url.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = uri.authority()?.clone();
    let host = authority
        .host()
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or_else(|| authority.host());
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .ok()
            .is_some_and(|address| address.is_loopback());
    loopback.then(|| SetupLaunchOrigin {
        scheme: scheme.to_ascii_lowercase(),
        authority,
    })
}

fn request_host_matches_configured(headers: &HeaderMap, configured: &SetupLaunchOrigin) -> bool {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok())
        .is_some_and(|authority| {
            authority
                .as_str()
                .eq_ignore_ascii_case(configured.authority.as_str())
        })
}

/// Match the browser's serialized `Origin` exactly to `DJINN_PUBLIC_URL`'s
/// origin. Comparing against configured state (not merely Origin vs Host)
/// prevents DNS rebinding and catches localhost/127.0.0.1 alias drift before
/// GitHub sends the callback to a host that lacks the setup cookie.
fn request_origin_matches_configured(headers: &HeaderMap, configured: &SetupLaunchOrigin) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(uri) = origin.parse::<Uri>() else {
        return false;
    };
    if !uri
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case(&configured.scheme))
    {
        return false;
    }
    if uri
        .path_and_query()
        .is_some_and(|value| value.as_str() != "/")
    {
        return false;
    }
    uri.authority().is_some_and(|authority| {
        authority
            .as_str()
            .eq_ignore_ascii_case(configured.authority.as_str())
    })
}

/// `/auth/config` is a GET, so browsers normally omit `Origin`. The
/// browser fetch-metadata header plus exact configured Host are the issuance
/// gate; if an Origin is present, it must also equal the configured origin.
fn same_origin_config_fetch(headers: &HeaderMap, configured: &SetupLaunchOrigin) -> bool {
    sec_fetch_site_is_same_origin(headers)
        && request_host_matches_configured(headers, configured)
        && (!headers.contains_key(header::ORIGIN)
            || request_origin_matches_configured(headers, configured))
}

fn local_setup_launch_available(headers: &HeaderMap, configured_public_url: &str) -> bool {
    configured_loopback_setup_origin(configured_public_url)
        .as_ref()
        .is_some_and(|configured| same_origin_config_fetch(headers, configured))
}

/// The form POST has both browser signals. Require both so a same-site page
/// on another localhost port cannot ride the Strict cookie into setup, and
/// bind both signals to the configured callback origin.
fn same_origin_setup_post(headers: &HeaderMap, configured: &SetupLaunchOrigin) -> bool {
    sec_fetch_site_is_same_origin(headers)
        && request_host_matches_configured(headers, configured)
        && request_origin_matches_configured(headers, configured)
}

/// Set the install-continuation cookie, which carries the manifest-flow
/// nonce through the cross-domain GitHub install round-trip.
///
/// Cookie properties:
/// - HttpOnly, SameSite=Lax
/// - Path-scoped to `/auth/github` (covers both `callback` and `app-setup-callback`)
/// - Secure when `DJINN_PUBLIC_URL` is HTTPS
/// - Expires after 10 minutes
fn set_install_continuation_cookie(headers: &mut HeaderMap, value: &str) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie = format!(
        "{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
        name = INSTALL_CONTINUATION_COOKIE,
        path = SETUP_SESSION_PATH,
        max_age = INSTALL_CONTINUATION_TTL_SECS,
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

/// Clear the install-continuation cookie.
fn clear_install_continuation_cookie(headers: &mut HeaderMap) {
    let secure = if cookie_secure() { "; Secure" } else { "" };
    let cookie = format!(
        "{name}=; Path={path}; HttpOnly; SameSite=Lax; Max-Age=0; \
         Expires=Thu, 01 Jan 1970 00:00:00 GMT{secure}",
        name = INSTALL_CONTINUATION_COOKIE,
        path = SETUP_SESSION_PATH,
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, hv);
    }
}

/// Guard for setup routes: returns `Some(404 response)` when the self-setup
/// gate is closed (disabled or state is anything except truly unconfigured),
/// or `None` when setup is available and the handler should proceed.
async fn setup_route_guard(state: &AppState) -> Option<Response> {
    let credential_state = state.app_credential_state().await;
    if !setup_available(&credential_state) {
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

#[derive(Debug, PartialEq, Eq)]
enum OAuthCallbackOrigin {
    /// OAuth explicitly started by `/auth/github/start`, protected by the
    /// normal state parameter + HttpOnly state cookie pair.
    Stateful {
        want_install: bool,
        redirect: String,
    },
    /// OAuth automatically started by GitHub after App installation. GitHub
    /// documents this callback as `code`-only, so the pending manifest
    /// continuation cookie is the CSRF correlation instead.
    InstallInitiated { continuation: String },
}

async fn resolve_oauth_callback_origin(
    state: &AppState,
    q: &CallbackQuery,
    headers: &HeaderMap,
) -> Result<(String, OAuthCallbackOrigin), Response> {
    let Some(code) = q.code.as_deref().filter(|code| !code.is_empty()) else {
        return Err((StatusCode::BAD_REQUEST, "missing code").into_response());
    };

    if let Some(state_param) = q.state.as_deref().filter(|state| !state.is_empty()) {
        let Some(cookie_raw) = extract_cookie(headers, OAUTH_STATE_COOKIE) else {
            return Err((StatusCode::BAD_REQUEST, "missing state cookie").into_response());
        };
        // Cookie format: `<state>|i0|<redirect>` or
        // `<state>|i1|<redirect>`. Legacy `<state>|<redirect>` remains valid
        // for callbacks already in flight during an upgrade.
        let mut parts = cookie_raw.splitn(3, '|');
        let cookie_state = parts.next().unwrap_or("");
        let (want_install, redirect) = match (parts.next(), parts.next()) {
            (Some("i1"), Some(redirect)) => (true, redirect.to_string()),
            (Some("i0"), Some(redirect)) => (false, redirect.to_string()),
            (Some(redirect), None) => (false, redirect.to_string()),
            _ => (false, "/".to_string()),
        };
        if !constant_time_eq(cookie_state.as_bytes(), state_param.as_bytes()) {
            return Err((StatusCode::BAD_REQUEST, "state mismatch").into_response());
        }
        return Ok((
            code.to_string(),
            OAuthCallbackOrigin::Stateful {
                want_install,
                redirect,
            },
        ));
    }

    let Some(continuation) = extract_cookie(headers, INSTALL_CONTINUATION_COOKIE) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "missing OAuth state or install continuation cookie",
        )
            .into_response());
    };
    if !state
        .validate_pending_install_continuation(&continuation)
        .await
    {
        return Err((
            StatusCode::FORBIDDEN,
            "invalid or expired install continuation",
        )
            .into_response());
    }

    Ok((
        code.to_string(),
        OAuthCallbackOrigin::InstallInitiated { continuation },
    ))
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
    if q.code.as_deref().filter(|code| !code.is_empty()).is_none()
        && let Some(installation_id) = q.installation_id.as_ref()
        && q.setup_action.as_deref() == Some("install")
    {
        let mut resp_headers = HeaderMap::new();
        // The install-continuation cookie (set by the manifest callback)
        // travels with the browser through GitHub's cross-domain redirect.
        // Forward its value as a query param so `app_setup_callback` can
        // validate the continuation nonce. GitHub does not echo custom query
        // params on its install redirect, so the cookie is the transport.
        let continuation_qs = extract_cookie(&headers, INSTALL_CONTINUATION_COOKIE)
            .map(|c| {
                format!(
                    "&{p}={v}",
                    p = INSTALL_CONTINUATION_PARAM,
                    v = urlencode(&c)
                )
            })
            .unwrap_or_default();
        let target = format!(
            "{}/auth/github/app-setup-callback?installation_id={}&setup_action=install{cont}",
            public_url().trim_end_matches('/'),
            urlencode(installation_id),
            cont = continuation_qs,
        );
        resp_headers.insert(
            header::LOCATION,
            HeaderValue::from_str(&target).unwrap_or_else(|_| HeaderValue::from_static("/")),
        );
        return (StatusCode::FOUND, resp_headers).into_response();
    }

    let (code, callback_origin) = match resolve_oauth_callback_origin(&state, &q, &headers).await {
        Ok(origin) => origin,
        Err(response) => return response,
    };
    let (want_install, redirect, install_continuation) = match &callback_origin {
        OAuthCallbackOrigin::Stateful {
            want_install,
            redirect,
        } => (*want_install, redirect.clone(), None),
        OAuthCallbackOrigin::InstallInitiated { continuation } => {
            (false, "/".to_string(), Some(continuation.clone()))
        }
    };

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
    let tokens = match exchange_user_code(&client_id, &client_secret, &code, &callback_url).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "auth callback: token exchange failed");
            return (StatusCode::BAD_GATEWAY, "token exchange failed").into_response();
        }
    };
    let access_token = tokens.access_token.clone();

    // GitHub's install-triggered OAuth callback is not correlated with
    // Djinn's normal OAuth `state`, so it must never create a browser session
    // or bootstrap admin. Use the short-lived user token only to discover the
    // just-authorized installation, then bind it through the authoritative
    // App-JWT callback. Session creation happens in a second, stateful OAuth
    // round-trip after binding.
    if let Some(continuation) = install_continuation.as_deref() {
        let installation_id = match list_user_installations(&access_token).await {
            Ok(installations) => {
                unique_selectable_installation_id(installations, allow_user_installations())
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "install OAuth callback: user installation discovery failed; using picker",
                );
                None
            }
        };

        let mut response_headers = HeaderMap::new();
        let location = if let Some(installation_id) = installation_id {
            format!(
                "{}/auth/github/app-setup-callback?installation_id={installation_id}\
                 &setup_action=install&{param}={nonce}",
                public_url().trim_end_matches('/'),
                param = INSTALL_CONTINUATION_PARAM,
                nonce = urlencode(continuation),
            )
        } else {
            // Zero or multiple installations are ambiguous. The existing
            // setup picker performs the explicit choice. Keep the correlated
            // nonce pending and move its browser copy into a cookie scoped to
            // the picker endpoints. GET/POST validate that bearer capability,
            // and POST consumes it only after the atomic org binding succeeds.
            // This preserves the code-only no-session/no-admin invariant
            // without exposing an unauthenticated first-binding surface.
            clear_install_continuation_cookie(&mut response_headers);
            crate::server::github_install::set_picker_capability_cookie(
                &mut response_headers,
                continuation,
            );
            format!("{}/", web_url().trim_end_matches('/'))
        };
        response_headers.insert(
            header::LOCATION,
            HeaderValue::from_str(&location).unwrap_or_else(|_| HeaderValue::from_static("/")),
        );
        return (StatusCode::FOUND, response_headers).into_response();
    }

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
    //    No OAuth mode may create a session before this binding exists. In
    //    particular, the public `install=1` convenience flag cannot become a
    //    bootstrap-admin bypass; the manifest flow binds first, then starts a
    //    separate stateful OAuth callback.
    let org_cfg = match load_org_config_for_auth(&state).await {
        Ok(Some(cfg)) => Some(cfg),
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
    // A deployment bound to a personal account has no organization
    // membership endpoint to consult. With the explicit opt-in enabled, only
    // the exact bound GitHub identity (immutable id + login) is accepted.
    let is_bound_personal_account = org_cfg.as_ref().is_some_and(|cfg| {
        allow_user_installations() && binding_matches_user(cfg, user.id as i64, &user.login)
    });

    if let Some(cfg) = org_cfg.as_ref().filter(|_| !is_bound_personal_account) {
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
    // are manual `UPDATE users SET is_admin = …`. The repository method uses a
    // transaction-scoped advisory lock so concurrent first logins cannot both
    // win. Best-effort: a failure here must not block sign-in (the next login
    // retries while no admin exists).
    if !user_row.is_admin {
        match users_repo.grant_bootstrap_admin_if_none(&user_row.id).await {
            Ok(true) => {
                tracing::info!(user_id = %user_row.id, login = %user.login, "auth callback: stamped first user as admin");
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, user_id = %user_row.id, "auth callback: bootstrap admin grant failed; skipping bootstrap");
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

    // 7. Build redirect response with cookies.
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
            .or_else(djinn_provider::github_app::app_slug);
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

fn unique_selectable_installation_id(
    installations: Vec<djinn_provider::github_app::Installation>,
    allow_users: bool,
) -> Option<u64> {
    let mut selectable = installations.into_iter().filter(|installation| {
        installation_account_type_allowed(&installation.account_type, allow_users)
    });
    let installation = selectable.next()?;
    selectable.next().is_none().then_some(installation.id)
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct GhUser {
    id: u64,
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

async fn fetch_github_user(access_token: &str) -> Result<GhUser, String> {
    #[cfg(test)]
    if let Some(result) = GITHUB_USER_RESULT_OVERRIDE.lock().unwrap().clone() {
        return result;
    }
    let user = GitHubServerClient::new().fetch_user(access_token).await?;
    Ok(GhUser {
        id: user.id,
        login: user.login,
        name: user.name,
        avatar_url: user.avatar_url,
    })
}

async fn exchange_user_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<GithubUserTokens, String> {
    #[cfg(test)]
    if let Some(result) = OAUTH_EXCHANGE_RESULT_OVERRIDE.lock().unwrap().clone() {
        return result;
    }
    github_app_user::exchange_code(client_id, client_secret, code, redirect_uri)
        .await
        .map_err(|error| error.to_string())
}

async fn list_user_installations(
    access_token: &str,
) -> Result<Vec<djinn_provider::github_app::Installation>, String> {
    #[cfg(test)]
    if let Some(result) = USER_INSTALLATIONS_RESULT_OVERRIDE.lock().unwrap().clone() {
        return result;
    }
    djinn_provider::github_app::list_installations_for_user(access_token)
        .await
        .map_err(|error| error.to_string())
}

async fn load_org_config_for_auth(state: &AppState) -> Result<Option<OrgConfig>, String> {
    #[cfg(test)]
    if let Some(result) = ORG_CONFIG_RESULT_OVERRIDE.lock().unwrap().clone() {
        return result;
    }
    OrgConfigRepository::new(state.db().clone())
        .get()
        .await
        .map_err(|error| error.to_string())
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
    djinn_provider::github_app::public_url()
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

/// Copy all `Set-Cookie` headers from `src` into `dest`'s response headers.
/// Used when a handler builds a redirect response and then needs to append
/// extra cookie-clearing directives from a separate `HeaderMap`.
fn merge_set_cookie(dest: &mut Response, src: &HeaderMap) {
    for hv in src.get_all(header::SET_COOKIE) {
        dest.headers_mut().append(header::SET_COOKIE, hv.clone());
    }
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
// GitHub sends the browser here after an App installation has been resolved
// by the OAuth callback or selected through the existing installation picker.
// This callback authoritatively resolves the installation with the App JWT
// and binds the selected account to this deployment.

/// Query parameters for `GET /auth/github/app-setup-callback` — GitHub
/// appends `?installation_id=<N>&setup_action=install` after the user
/// completes (or requests) an installation via the App's install page.
///
/// When the request originates from the manifest-continuation flow (i.e.
/// after credential persistence), an additional `djinn_continuation` param
/// is present and must match the pending continuation nonce.
#[derive(Deserialize)]
struct AppSetupQuery {
    installation_id: Option<String>,
    #[serde(default)]
    setup_action: Option<String>,
    /// Install-continuation nonce appended by the manifest callback redirect.
    #[serde(default, rename = "djinn_continuation")]
    continuation_state: Option<String>,
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
    headers: HeaderMap,
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

    // Validate install-continuation state when a manifest flow just completed.
    // Creating the initial deployment binding always requires the matching
    // nonce. A no-nonce callback is accepted only as an idempotent replay for
    // the exact installation already stored in org_config; otherwise this
    // public callback would bypass the picker capability and let a drive-by
    // request win the first-binding race.
    //
    // The nonce can arrive via:
    //   1. The `djinn_continuation` query param — set by `github_callback`
    //      when it bridges the install redirect to this endpoint (the nonce
    //      was carried through the GitHub round-trip in a cookie).
    //   2. The `djinn_install_continuation` cookie directly — for direct
    //      hits to this endpoint (e.g. GitHub's `setup_url` config).
    let continuation_cookie = extract_cookie(&headers, INSTALL_CONTINUATION_COOKIE);
    let continuation_candidate = q
        .continuation_state
        .as_deref()
        .or(continuation_cookie.as_deref());
    let continuation_pending = state.has_pending_install_continuation().await;
    let uncorrelated_idempotent_replay = if !continuation_pending
        && continuation_candidate.is_none()
    {
        match OrgConfigRepository::new(state.db().clone()).get().await {
            Ok(Some(existing)) => existing.installation_id as u64 == installation_id,
            Ok(None) => false,
            Err(error) => {
                tracing::error!(%error, "app_setup_callback: org_config read failed during continuation check");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    } else {
        false
    };
    let continuation_valid = match (continuation_pending, continuation_candidate) {
        (true, Some(candidate)) => state.validate_pending_install_continuation(candidate).await,
        (false, None) => uncorrelated_idempotent_replay,
        // A pending manifest flow always requires the nonce; a nonce with no
        // pending flow is stale or replayed and must not turn into an
        // unrestricted non-manifest callback.
        _ => false,
    };
    if !continuation_valid {
        tracing::warn!(
            installation_id,
            "app_setup_callback: install-continuation state mismatch or missing"
        );
        // Clear the cookie on failure to prevent retry confusion.
        let mut resp_headers = HeaderMap::new();
        clear_install_continuation_cookie(&mut resp_headers);
        return (
            StatusCode::FORBIDDEN,
            resp_headers,
            "install-continuation state mismatch — restart the setup flow",
        )
            .into_response();
    }

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

    if !installation_account_type_allowed(
        &installation.account().account_type,
        allow_user_installations(),
    ) {
        tracing::warn!(
            installation_id,
            account_type = %installation.account().account_type,
            account_login = %installation.account().login,
            "app_setup_callback: rejecting unsupported installation account type",
        );
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "This deployment does not allow GitHub account type '{}' for installation \
                 {installation_id}. Organization installations are always supported; User \
                 installations require DJINN_ALLOW_USER_INSTALLATIONS=true.",
                installation.account().account_type,
            ),
        )
            .into_response();
    }

    if installation
        .account()
        .account_type
        .eq_ignore_ascii_case("User")
    {
        tracing::info!(
            installation_id,
            account_login = %installation.account().login,
            "app_setup_callback: accepting explicitly enabled personal-account installation",
        );
    }

    // Keep the stored `org_config` shape for compatibility. Personal bindings
    // are distinguished later by exact account-id/login matching rather than
    // by pretending GitHub exposes an organization membership roster.

    let org_repo = OrgConfigRepository::new(state.db().clone());
    let matches_installation = |existing: &OrgConfig| {
        existing.installation_id as u64 == installation_id
            && existing.github_org_id as u64 == installation.account().id
    };

    // Setup callbacks are one-shot. A repeat callback for the exact binding
    // is idempotent, but a different installation must never replace an
    // established deployment binding.
    let already_bound = match org_repo.get().await {
        Ok(Some(existing)) if matches_installation(&existing) => true,
        Ok(Some(_)) => {
            return (
                StatusCode::CONFLICT,
                "This deployment is already bound to a different GitHub installation.",
            )
                .into_response();
        }
        Ok(None) => false,
        Err(error) => {
            tracing::error!(error = %error, "app_setup_callback: org_config read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if !already_bound {
        match org_repo
            .create_if_absent(NewOrgConfig {
                github_org_id: installation.account().id as i64,
                github_org_login: &installation.account().login,
                app_id: cfg.app_id as i64,
                installation_id: installation_id as i64,
            })
            .await
        {
            Ok(Some(_)) => {
                tracing::info!(
                    installation_id,
                    account = %installation.account().login,
                    action,
                    "app_setup_callback: org_config bound",
                );
            }
            Ok(None) => {
                // Another callback won the insert race. Treat an identical
                // winner as an idempotent retry; reject any different binding.
                match org_repo.get().await {
                    Ok(Some(existing)) if matches_installation(&existing) => {}
                    Ok(Some(_)) => {
                        return (
                            StatusCode::CONFLICT,
                            "This deployment was concurrently bound to a different GitHub installation.",
                        )
                            .into_response();
                    }
                    Ok(None) => {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "app_setup_callback: org_config race read failed");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                }
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    installation_id,
                    account = %installation.account().login,
                    "app_setup_callback: org_config create failed",
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to persist org binding. Check server logs.",
                )
                    .into_response();
            }
        }
    }

    if already_bound {
        tracing::info!(
            installation_id,
            action,
            "app_setup_callback: idempotent re-entry for existing binding",
        );
    }
    if continuation_pending
        && !state
            .consume_pending_install_continuation(
                continuation_candidate.expect("validated continuation candidate"),
            )
            .await
    {
        return (
            StatusCode::CONFLICT,
            "install continuation was already consumed — restart setup",
        )
            .into_response();
    }
    let mut resp = redirect_after_install_binding(continuation_pending);
    let mut extra = HeaderMap::new();
    clear_install_continuation_cookie(&mut extra);
    merge_set_cookie(&mut resp, &extra);
    resp
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

/// A manifest-origin install callback has only the install-continuation CSRF
/// proof, not an OAuth `state`. Once the authoritative App-JWT binding is
/// persisted, start the normal stateful OAuth flow; only that later callback
/// may create a browser session or bootstrap the first admin.
fn redirect_after_install_binding(manifest_origin: bool) -> Response {
    if !manifest_origin {
        return redirect_to_web();
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_static("/auth/github/start?redirect=%2F"),
    );
    (StatusCode::FOUND, headers).into_response()
}

/// Fetch an installation's account info via the App JWT.
async fn fetch_installation_for_setup(installation_id: u64) -> Result<AppInstallation, String> {
    #[cfg(test)]
    if let Some(result) = APP_INSTALLATION_RESULT_OVERRIDE.lock().unwrap().clone() {
        return result;
    }
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
    credential_source: Option<&'static str>,
    setup_state: &'static str,
    setup_error: Option<String>,
    setup_retryable: bool,
    credentials_unrecoverable: bool,
}

async fn setup_status(
    State(state): State<AppState>,
) -> Result<Json<SetupStatusResponse>, (StatusCode, &'static str)> {
    let credential_state = state.app_credential_state().await;
    let app_cfg = credential_state.app_config();
    let status = credential_status_fields(&credential_state);
    let org_cfg = OrgConfigRepository::new(state.db().clone())
        .get()
        .await
        .map_err(|error| {
            tracing::error!(%error, "setup status: failed to read installation binding");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read GitHub installation binding",
            )
        })?;
    let needs_app_install = app_cfg.is_none() || org_cfg.is_none();
    let app_credentials_configured = app_cfg.is_some();
    let org_login = org_cfg.map(|c| c.github_org_login);

    Ok(Json(SetupStatusResponse {
        needs_app_install,
        app_credentials_configured,
        org_login,
        credential_source: status.credential_source,
        setup_state: status.setup_state,
        setup_error: status.setup_error,
        setup_retryable: status.setup_retryable,
        credentials_unrecoverable: status.credentials_unrecoverable,
    }))
}

// ─── Self-setup create-app + manifest-callback handlers ───────────────────────

/// `POST /auth/github/setup-start` — explicit browser-only entry point for
/// the local onboarding CTA.
///
/// `/auth/config` issues a short-lived HttpOnly, SameSite=Strict capability
/// only to a same-origin browser fetch. This endpoint requires that cookie,
/// independently checks the browser's same-origin fetch metadata + Origin,
/// consumes the capability, and only then establishes the Lax setup session
/// needed for GitHub's cross-site callback.
async fn setup_start(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(resp) = setup_route_guard(&state).await {
        return resp;
    }

    let Some(configured_origin) = configured_loopback_setup_origin(&public_url()) else {
        // Remote deployments retain the explicit boot-token/operator path,
        // but never expose the browser-mintable setup authority.
        return StatusCode::NOT_FOUND.into_response();
    };

    // Validate the browser context before looking up or consuming the token.
    // A cross-site request must not be able to burn the legitimate operator's
    // one-shot capability as a denial of service.
    if !same_origin_setup_post(&headers, &configured_origin) {
        return (
            StatusCode::FORBIDDEN,
            "same-origin browser request required",
        )
            .into_response();
    }

    let Some(launch_token) = extract_cookie(&headers, SETUP_LAUNCH_COOKIE) else {
        return setup_launch_expired_redirect();
    };
    if !state.consume_setup_launch_capability(&launch_token).await {
        return setup_launch_expired_redirect();
    }

    let existing_setup_cookie = extract_cookie(&headers, SETUP_SESSION_COOKIE);
    let session_value = state
        .begin_browser_setup_session(existing_setup_cookie.as_deref())
        .await;
    let mut response = render_manifest_form();
    set_setup_cookie(response.headers_mut(), &session_value);
    clear_setup_launch_cookie(response.headers_mut());
    response
}

/// Query parameters for `GET /auth/github/create-app`.
///
/// When `setup_token` is present, this is a boot-token exchange request.
/// Otherwise, the caller must present a valid `djinn_setup_session` cookie.
#[derive(Deserialize)]
struct CreateAppQuery {
    setup_token: Option<String>,
}

/// Render the GitHub App manifest form shared by the boot-token fallback and
/// the browser setup CTA. The form auto-submits on navigation; no Djinn
/// credential or setup capability is included in the document.
fn render_manifest_form() -> Response {
    let public = public_url();
    // GitHub App names are globally unique. Generate a fresh suffix for every
    // manifest form so a retry or a second local operator does not collide on
    // the old deterministic `djinn-localhost` name. This randomness is
    // independent from the boot/setup token.
    let manifest = build_manifest_json(&public, &manifest_name_suffix());
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

    render_manifest_form()
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

    state.clear_setup_session_token().await;

    // Generate an install-continuation nonce and store it so that
    // `/auth/github/app-setup-callback` can validate the redirect came from
    // this manifest flow (not a direct/unsolicited hit).
    let continuation_nonce = random_token_b64();
    state
        .set_pending_install_continuation(continuation_nonce.clone())
        .await;

    // Build the install URL for the newly created App, appending the
    // continuation nonce as a query parameter.
    let base_install_url = cfg
        .install_url()
        .unwrap_or_else(|| format!("{}/", web_url().trim_end_matches('/')));
    let install_url = if base_install_url.contains('?') {
        format!(
            "{base_install_url}&{param}={nonce}",
            param = INSTALL_CONTINUATION_PARAM,
            nonce = urlencode(&continuation_nonce),
        )
    } else {
        format!(
            "{base_install_url}?{param}={nonce}",
            param = INSTALL_CONTINUATION_PARAM,
            nonce = urlencode(&continuation_nonce),
        )
    };

    // Clear all setup cookies/state: manifest CSRF + setup session.
    // Simultaneously set the install-continuation cookie so the nonce
    // survives the cross-domain GitHub install round-trip.
    let mut resp_headers = HeaderMap::new();
    clear_cookie(&mut resp_headers, MANIFEST_STATE_COOKIE);
    clear_setup_cookie(&mut resp_headers);
    set_install_continuation_cookie(&mut resp_headers, &continuation_nonce);
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
/// - no webhook configuration (Djinn does not consume GitHub webhooks)
/// - permissions match the repo, CI-status, and org-membership APIs Djinn uses
pub(crate) fn build_manifest_json(public_url: &str, name_suffix: &str) -> serde_json::Value {
    // GitHub automatically grants `metadata: read` as a base permission; we
    // only list the permissions we explicitly need.
    let permissions = serde_json::json!({
        "actions": "read",
        "checks": "read",
        "contents": "write",
        "members": "read",
        "pull_requests": "write",
    });
    serde_json::json!({
        "name": format!(
            "djinn-{}-{}",
            url_host(public_url).unwrap_or_else(|| "local".to_string()),
            name_suffix
        ),
        "url": public_url,
        "redirect_url": format!("{}/auth/github/app-manifest-callback", public_url),
        "callback_urls": [format!("{}/auth/github/callback", public_url)],
        "request_oauth_on_install": true,
        "public": false,
        "default_permissions": permissions,
    })
}

fn manifest_name_suffix() -> String {
    random_token_b64()
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(8)
        .collect()
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

    djinn_provider::github_app::exchange_manifest_code(code)
        .await
        .map_err(|e| e.to_string())
}

/// Test-only override for `exchange_manifest_code`. When set to `Some(...)`,
/// the exchange function returns the overridden result instead of calling the
/// GitHub API. This allows tests to simulate exchange success/failure without
/// network access.
#[cfg(test)]
static EXCHANGE_MANIFEST_RESULT_OVERRIDE: std::sync::Mutex<
    Option<Result<ManifestConversion, String>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static OAUTH_EXCHANGE_RESULT_OVERRIDE: std::sync::Mutex<Option<Result<GithubUserTokens, String>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static GITHUB_USER_RESULT_OVERRIDE: std::sync::Mutex<Option<Result<GhUser, String>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static USER_INSTALLATIONS_RESULT_OVERRIDE: std::sync::Mutex<
    Option<Result<Vec<djinn_provider::github_app::Installation>, String>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static APP_INSTALLATION_RESULT_OVERRIDE: std::sync::Mutex<Option<Result<AppInstallation, String>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static ORG_CONFIG_RESULT_OVERRIDE: std::sync::Mutex<Option<Result<Option<OrgConfig>, String>>> =
    std::sync::Mutex::new(None);

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

    fn same_origin_config_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8372"));
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        headers
    }

    fn same_origin_setup_headers(launch_token: &str) -> HeaderMap {
        let mut headers = same_origin_config_headers();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8372"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SETUP_LAUNCH_COOKIE}={launch_token}")).unwrap(),
        );
        headers
    }

    fn set_cookie_value(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
        for value in headers.get_all(header::SET_COOKIE) {
            let raw = value.to_str().ok()?;
            let pair = raw.split(';').next()?;
            let (name, value) = pair.split_once('=')?;
            if name == cookie_name {
                return Some(value.to_string());
            }
        }
        None
    }

    fn assert_setup_expired_recovery(response: &Response) {
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            Some("/?setup=expired")
        );
        assert_eq!(
            set_cookie_value(response.headers(), SETUP_LAUNCH_COOKIE).as_deref(),
            Some(""),
            "recovery must clear the stale launch cookie"
        );
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|cookie| {
                    cookie.starts_with(&format!("{SETUP_LAUNCH_COOKIE}="))
                        && cookie.contains("Max-Age=0")
                }),
            "recovery must expire the launch cookie"
        );
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
    fn personal_installation_policy_is_default_deny_and_user_only() {
        assert!(!parse_allow_user_installations(None));
        assert!(!parse_allow_user_installations(Some("false")));
        assert!(parse_allow_user_installations(Some(" true ")));
        assert!(parse_allow_user_installations(Some("1")));

        assert!(installation_account_type_allowed("Organization", false));
        assert!(!installation_account_type_allowed("User", false));
        assert!(installation_account_type_allowed("User", true));
        assert!(!installation_account_type_allowed("Bot", true));
        assert!(!installation_account_type_allowed("Enterprise", true));
    }

    #[test]
    fn personal_binding_requires_exact_id_and_login() {
        let binding = OrgConfig {
            id: 1,
            github_org_id: 42,
            github_org_login: "OctoCat".into(),
            app_id: 7,
            installation_id: 9,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(binding_matches_user(&binding, 42, "octocat"));
        assert!(!binding_matches_user(&binding, 43, "octocat"));
        assert!(!binding_matches_user(&binding, 42, "someone-else"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_initiated_code_only_callback_uses_continuation_without_oauth_state_cookie() {
        use crate::test_helpers;

        let state = test_helpers::test_app_state_in_memory().await;
        let continuation = "manifest-install-continuation";
        state
            .set_pending_install_continuation_for_tests(Some(continuation.to_string()))
            .await;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{INSTALL_CONTINUATION_COOKIE}={continuation}"))
                .unwrap(),
        );
        assert!(extract_cookie(&headers, OAUTH_STATE_COOKIE).is_none());

        let query = CallbackQuery {
            code: Some("install-oauth-code".into()),
            state: None,
            installation_id: None,
            setup_action: None,
        };
        let origin = resolve_oauth_callback_origin(&state, &query, &headers)
            .await
            .expect("valid install continuation must authenticate a code-only callback");
        assert_eq!(
            origin,
            (
                "install-oauth-code".to_string(),
                OAuthCallbackOrigin::InstallInitiated {
                    continuation: continuation.to_string(),
                },
            )
        );
        assert!(
            state
                .validate_pending_install_continuation(continuation)
                .await,
            "classification must not consume the retry nonce before binding/transition"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_initiated_oauth_routes_unique_installation_without_session_or_admin() {
        use crate::test_helpers;
        use djinn_provider::github_app::Installation;

        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_config(Some(Arc::new(djinn_provider::github_app::AppConfig {
                app_id: 42,
                slug: "djinn-install-test".into(),
                client_id: "Iv1.install-test".into(),
                client_secret: "install-secret".into(),
                pem: "PEM".into(),
                webhook_secret: String::new(),
                public_url: "http://127.0.0.1:8372".into(),
            })))
            .await;

        let continuation = "install-oauth-roundtrip";
        state
            .set_pending_install_continuation_for_tests(Some(continuation.into()))
            .await;
        *OAUTH_EXCHANGE_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(GithubUserTokens {
            access_token: "ghu_install_user".into(),
            expires_in: Some(28_800),
            refresh_token: Some("ghr_install_user".into()),
            refresh_token_expires_in: Some(15_897_600),
        }));
        *USER_INSTALLATIONS_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(vec![Installation {
            id: 777,
            account_login: "acme".into(),
            account_type: "Organization".into(),
            target_type: "Organization".into(),
        }]));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{INSTALL_CONTINUATION_COOKIE}={continuation}"))
                .unwrap(),
        );
        let response = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: Some("github-install-code".into()),
                state: None,
                installation_id: None,
                setup_action: None,
            }),
            headers,
        )
        .await;

        assert_eq!(response.status(), StatusCode::FOUND);
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(location.contains("/auth/github/app-setup-callback"));
        assert!(location.contains("installation_id=777"));
        assert!(location.contains(&format!("{INSTALL_CONTINUATION_PARAM}={continuation}")));
        assert!(
            !response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|cookie| cookie.starts_with(&format!("{SESSION_COOKIE}="))),
            "code-only install OAuth must not create a browser session"
        );
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .all(|cookie| !cookie.starts_with(&format!("{OAUTH_STATE_COOKIE}="))),
            "install-triggered callback does not mint normal OAuth state retroactively"
        );
        assert!(
            state
                .validate_pending_install_continuation(continuation)
                .await,
            "nonce remains pending until the authoritative binding callback succeeds"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ambiguous_install_oauth_issues_picker_capability_without_session_or_admin() {
        use crate::test_helpers;
        use djinn_provider::github_app::Installation;

        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_config(Some(Arc::new(djinn_provider::github_app::AppConfig {
                app_id: 42,
                slug: "djinn-picker-test".into(),
                client_id: "Iv1.picker-test".into(),
                client_secret: "picker-secret".into(),
                pem: "PEM".into(),
                webhook_secret: String::new(),
                public_url: "http://127.0.0.1:8372".into(),
            })))
            .await;

        let continuation = "ambiguous-install-continuation";
        state
            .set_pending_install_continuation_for_tests(Some(continuation.into()))
            .await;
        *OAUTH_EXCHANGE_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(GithubUserTokens {
            access_token: "ghu_ambiguous_install_user".into(),
            expires_in: Some(28_800),
            refresh_token: Some("ghr_ambiguous_install_user".into()),
            refresh_token_expires_in: Some(15_897_600),
        }));
        *USER_INSTALLATIONS_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(vec![
            Installation {
                id: 777,
                account_login: "acme".into(),
                account_type: "Organization".into(),
                target_type: "Organization".into(),
            },
            Installation {
                id: 888,
                account_login: "other-org".into(),
                account_type: "Organization".into(),
                target_type: "Organization".into(),
            },
        ]));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{INSTALL_CONTINUATION_COOKIE}={continuation}"))
                .unwrap(),
        );
        let response = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: Some("github-ambiguous-install-code".into()),
                state: None,
                installation_id: None,
                setup_action: None,
            }),
            headers,
        )
        .await;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "http://127.0.0.1:8372/"
        );
        let picker_cookie = extract_set_cookie_value(
            &response,
            crate::server::github_install::INSTALL_PICKER_CAPABILITY_COOKIE,
        )
        .expect("ambiguous continuation must become a picker capability");
        assert_eq!(picker_cookie, continuation);
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|cookie| {
                    cookie.starts_with(&format!(
                        "{}=",
                        crate::server::github_install::INSTALL_PICKER_CAPABILITY_COOKIE
                    )) && cookie.contains("Path=/api/github/installations")
                        && cookie.contains("HttpOnly")
                        && cookie.contains("SameSite=Lax")
                }),
            "picker capability must be HttpOnly, same-site, and path-scoped"
        );
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|cookie| {
                    cookie.starts_with(&format!("{INSTALL_CONTINUATION_COOKIE}="))
                        && cookie.contains("Max-Age=0")
                }),
            "the broader auth-path continuation cookie must be retired"
        );
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .all(|cookie| !cookie.starts_with(&format!("{SESSION_COOKIE}="))),
            "ambiguous code-only OAuth must not create a Djinn session"
        );
        assert_eq!(
            UserRepository::new(state.db().clone())
                .admin_count()
                .await
                .unwrap(),
            0,
            "ambiguous code-only OAuth must not bootstrap an admin"
        );
        assert!(
            state
                .validate_pending_install_continuation(continuation)
                .await,
            "server-held nonce must remain live until the authorized picker binds"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stateful_install_flag_cannot_bootstrap_an_unbound_deployment() {
        use crate::test_helpers;

        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_config(Some(Arc::new(djinn_provider::github_app::AppConfig {
                app_id: 42,
                slug: "djinn-install-test".into(),
                client_id: "Iv1.install-test".into(),
                client_secret: "install-secret".into(),
                pem: "PEM".into(),
                webhook_secret: String::new(),
                public_url: "http://127.0.0.1:8372".into(),
            })))
            .await;
        *OAUTH_EXCHANGE_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(GithubUserTokens {
            access_token: "ghu_unbound".into(),
            expires_in: None,
            refresh_token: None,
            refresh_token_expires_in: None,
        }));
        *GITHUB_USER_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(GhUser {
            id: 1234,
            login: "unbound-installer".into(),
            name: None,
            avatar_url: None,
        }));
        *ORG_CONFIG_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(None));

        let oauth_state = "stateful-install-attempt";
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{OAUTH_STATE_COOKIE}={oauth_state}|i1|/")).unwrap(),
        );
        let response = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: Some("stateful-install-code".into()),
                state: Some(oauth_state.into()),
                installation_id: None,
                setup_action: None,
            }),
            headers,
        )
        .await;

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .all(|cookie| !cookie.starts_with(&format!("{SESSION_COOKIE}=")))
        );
        *OAUTH_EXCHANGE_RESULT_OVERRIDE.lock().unwrap() = None;
        *GITHUB_USER_RESULT_OVERRIDE.lock().unwrap() = None;
        *ORG_CONFIG_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    #[test]
    fn install_oauth_auto_selects_only_one_allowed_installation() {
        use djinn_provider::github_app::Installation;

        let installation = |id, account_type: &str| Installation {
            id,
            account_login: format!("account-{id}"),
            account_type: account_type.into(),
            target_type: account_type.into(),
        };

        assert_eq!(
            unique_selectable_installation_id(vec![installation(7, "Organization")], false,),
            Some(7)
        );
        assert_eq!(
            unique_selectable_installation_id(
                vec![
                    installation(7, "Organization"),
                    installation(8, "Organization"),
                ],
                false,
            ),
            None,
            "multiple installations must land on the picker"
        );
        assert_eq!(
            unique_selectable_installation_id(vec![installation(9, "User")], false),
            None,
            "personal installs stay gated by the explicit opt-in"
        );
        assert_eq!(
            unique_selectable_installation_id(vec![installation(9, "User")], true),
            Some(9)
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
        let resp = setup_status(State(state)).await.unwrap();
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

        let resp = setup_status(State(state)).await.unwrap();
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

        let resp = setup_status(State(state)).await.unwrap();
        assert!(resp.0.needs_app_install);
        assert!(resp.0.app_credentials_configured);
        assert!(resp.0.org_login.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_status_reports_database_failure_instead_of_needs_install() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        state.db().pool().close().await;

        let error = match setup_status(State(state)).await {
            Ok(_) => panic!("closed database must not look like an unbound deployment"),
            Err(error) => error,
        };

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.1, "failed to read GitHub installation binding");
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
        let resp = config(State(state), HeaderMap::new()).await;
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
        let resp = config(State(state), HeaderMap::new()).await;
        assert!(
            set_cookie_value(&resp.1, SETUP_LAUNCH_COOKIE).is_none(),
            "non-browser config reads must not mint setup authority"
        );
        let body = resp.0;
        assert!(body.self_setup_available);
        assert!(!body.setup_launch_available);
    }

    #[test]
    fn setup_launch_accepts_exact_configured_loopback_origin() {
        let headers = same_origin_config_headers();
        assert!(local_setup_launch_available(
            &headers,
            "http://127.0.0.1:8372"
        ));
    }

    #[test]
    fn setup_launch_rejects_remote_public_url_even_with_matching_browser_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("djinn.example:8443"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://djinn.example:8443"),
        );
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(!local_setup_launch_available(
            &headers,
            "https://djinn.example:8443"
        ));
    }

    #[test]
    fn setup_launch_rejects_request_host_mismatch() {
        let mut headers = same_origin_config_headers();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:9999"));
        assert!(!local_setup_launch_available(
            &headers,
            "http://127.0.0.1:8372"
        ));
    }

    #[test]
    fn setup_launch_rejects_localhost_alias_for_127_callback() {
        let mut headers = same_origin_config_headers();
        headers.insert(header::HOST, HeaderValue::from_static("localhost:8372"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:8372"),
        );
        assert!(!local_setup_launch_available(
            &headers,
            "http://127.0.0.1:8372"
        ));
    }

    #[test]
    fn setup_launch_loopback_parser_accepts_localhost_ipv4_127_slash_8_and_ipv6() {
        assert!(configured_loopback_setup_origin("http://localhost:8372").is_some());
        assert!(configured_loopback_setup_origin("http://127.42.0.9:8372").is_some());
        assert!(configured_loopback_setup_origin("http://[::1]:8372").is_some());
        assert!(configured_loopback_setup_origin("http://10.0.0.1:8372").is_none());
    }

    /// A verified same-origin config fetch receives setup authority only in a
    /// tightly scoped HttpOnly cookie; the raw capability is absent from JSON.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_origin_config_fetch_issues_strict_path_scoped_launch_cookie() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let output = config(State(state), same_origin_config_headers()).await;
        let launch_token = set_cookie_value(&output.1, SETUP_LAUNCH_COOKIE)
            .expect("same-origin config fetch should receive launch cookie");
        assert_eq!(launch_token.len(), 43, "expected a 256-bit URL-safe token");

        let cookie = output
            .1
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(|value| {
                value
                    .to_str()
                    .ok()
                    .filter(|value| value.starts_with(SETUP_LAUNCH_COOKIE))
            })
            .expect("launch Set-Cookie header");
        assert!(cookie.contains("Path=/auth/github/setup-start"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=120"));

        let json = serde_json::to_string(&output.0).unwrap();
        assert!(output.0.self_setup_available);
        assert!(output.0.setup_launch_available);
        assert!(json.contains("\"setup_launch_available\":true"));
        assert!(
            !json.contains(&launch_token),
            "raw launch authority must never appear in /auth/config JSON"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_refuses_to_issue_launch_cookie_to_cross_origin_fetch() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let mut headers = same_origin_config_headers();
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        let output = config(State(state.clone()), headers).await;
        assert!(set_cookie_value(&output.1, SETUP_LAUNCH_COOKIE).is_none());
        assert!(!output.0.setup_launch_available);

        let mut same_site_wrong_origin = same_origin_config_headers();
        same_site_wrong_origin.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:8372"),
        );
        let output = config(State(state), same_site_wrong_origin).await;
        assert!(set_cookie_value(&output.1, SETUP_LAUNCH_COOKIE).is_none());
        assert!(!output.0.setup_launch_available);
    }

    /// The CTA exchanges a same-origin launch capability for the existing
    /// setup session and renders the manifest form exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_start_consumes_launch_and_renders_manifest_form() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let config_output = config(State(state.clone()), same_origin_config_headers()).await;
        let launch_token = set_cookie_value(&config_output.1, SETUP_LAUNCH_COOKIE).unwrap();
        let request_headers = same_origin_setup_headers(&launch_token);

        let response = setup_start(State(state.clone()), request_headers.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);

        let setup_session = set_cookie_value(response.headers(), SETUP_SESSION_COOKIE)
            .expect("setup session cookie");
        assert!(state.validate_setup_session_token(&setup_session).await);
        assert!(
            set_cookie_value(response.headers(), MANIFEST_STATE_COOKIE).is_some(),
            "manifest CSRF cookie must be minted"
        );
        assert_eq!(
            set_cookie_value(response.headers(), SETUP_LAUNCH_COOKIE).as_deref(),
            Some(""),
            "consumed launch cookie must be cleared"
        );

        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(html.contains("action=\"https://github.com/settings/apps/new"));
        assert!(!html.contains(&launch_token));
        assert!(
            !html.contains(&setup_session),
            "setup session authority must remain HttpOnly, never enter markup"
        );

        let replay = setup_start(State(state), request_headers).await;
        assert_setup_expired_recovery(&replay);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_start_missing_launch_cookie_redirects_to_spa_recovery() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        let mut headers = same_origin_setup_headers("unused");
        headers.remove(header::COOKIE);
        let response = setup_start(State(state), headers).await;

        assert_setup_expired_recovery(&response);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_start_expired_launch_cookie_redirects_to_spa_recovery() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let launch_token = state
            .issue_setup_launch_capability(std::time::Duration::ZERO)
            .await;

        let response = setup_start(State(state), same_origin_setup_headers(&launch_token)).await;

        assert_setup_expired_recovery(&response);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_start_replayed_launch_cookie_redirects_to_spa_recovery() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let launch_token = state
            .issue_setup_launch_capability(std::time::Duration::from_secs(120))
            .await;
        let headers = same_origin_setup_headers(&launch_token);

        let first = setup_start(State(state.clone()), headers.clone()).await;
        assert_eq!(first.status(), StatusCode::OK);
        let replay = setup_start(State(state), headers).await;

        assert_setup_expired_recovery(&replay);
    }

    /// Even when a test manually attaches the Strict cookie, cross-site or
    /// cross-origin browser signals are rejected before token consumption.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_start_rejects_drive_by_without_burning_capability() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        let launch_token = state
            .issue_setup_launch_capability(std::time::Duration::from_secs(120))
            .await;

        let mut cross_site = same_origin_setup_headers(&launch_token);
        cross_site.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        cross_site.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        let cross_site_response = setup_start(State(state.clone()), cross_site).await;
        assert_eq!(cross_site_response.status(), StatusCode::FORBIDDEN);
        assert!(
            cross_site_response
                .headers()
                .get(header::LOCATION)
                .is_none()
        );

        let mut wrong_origin = same_origin_setup_headers(&launch_token);
        wrong_origin.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:8372"),
        );
        let wrong_origin_response = setup_start(State(state.clone()), wrong_origin).await;
        assert_eq!(wrong_origin_response.status(), StatusCode::FORBIDDEN);
        assert!(
            wrong_origin_response
                .headers()
                .get(header::LOCATION)
                .is_none()
        );

        assert_eq!(
            setup_start(State(state), same_origin_setup_headers(&launch_token))
                .await
                .status(),
            StatusCode::OK,
            "rejected drive-by attempts must not consume the real capability"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_launch_capability_expires_server_side() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        let launch_token = state
            .issue_setup_launch_capability(std::time::Duration::ZERO)
            .await;
        assert!(!state.consume_setup_launch_capability(&launch_token).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_session_expires_server_side() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        let token = "expired-setup-session".to_string();
        state
            .set_setup_session_token_with_ttl_for_tests(
                Some(token.clone()),
                std::time::Duration::ZERO,
            )
            .await;

        assert!(!state.validate_setup_session_token(&token).await);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SETUP_SESSION_COOKIE}={token}")).unwrap(),
        );
        assert!(extract_setup_session(&headers, &state).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn browser_setup_reuses_only_matching_live_setup_cookie() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        let existing = "existing-live-setup-session".to_string();
        state
            .set_setup_session_token_for_tests(Some(existing.clone()))
            .await;

        assert_eq!(
            state.begin_browser_setup_session(Some(&existing)).await,
            existing
        );

        let replacement = state
            .begin_browser_setup_session(Some("wrong-setup-cookie"))
            .await;
        assert_ne!(replacement, existing);
        assert!(!state.validate_setup_session_token(&existing).await);
        assert!(state.validate_setup_session_token(&replacement).await);
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

        let resp = config(State(state), HeaderMap::new()).await;
        let body = resp.0;
        assert!(!body.self_setup_available);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_secret_is_retained_exposed_and_never_advertises_setup() {
        use crate::test_helpers;
        use djinn_provider::github_app::InvalidSecretDetail;

        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_credential_state_for_tests(CredentialSourceState::InvalidSecret(
                InvalidSecretDetail {
                    issues: vec!["GITHUB_APP_CLIENT_SECRET is empty"],
                },
            ))
            .await;

        let config_body = config(State(state.clone()), HeaderMap::new()).await.0;
        assert!(!config_body.configured);
        assert!(!config_body.self_setup_available);
        assert_eq!(config_body.credential_source, None);
        assert_eq!(config_body.setup_state, "invalid_secret");
        assert!(
            config_body
                .setup_error
                .as_deref()
                .is_some_and(|message| message.contains("GITHUB_APP_CLIENT_SECRET"))
        );
        assert!(!config_body.setup_retryable);
        assert!(!config_body.credentials_unrecoverable);

        let setup_body = setup_status(State(state.clone())).await.unwrap().0;
        assert_eq!(setup_body.setup_state, "invalid_secret");
        assert!(!setup_body.app_credentials_configured);
        assert!(!setup_body.credentials_unrecoverable);

        let response = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn undecryptable_persisted_state_is_exposed_and_setup_stays_hidden() {
        use crate::test_helpers;

        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_credential_state_for_tests(CredentialSourceState::UndecryptablePersisted)
            .await;

        let config_body = config(State(state.clone()), HeaderMap::new()).await.0;
        assert!(!config_body.self_setup_available);
        assert_eq!(config_body.setup_state, "credentials_unrecoverable");
        assert!(config_body.credentials_unrecoverable);
        assert!(config_body.setup_error.is_some());

        let setup_body = setup_status(State(state.clone())).await.unwrap().0;
        assert_eq!(setup_body.setup_state, "credentials_unrecoverable");
        assert!(setup_body.credentials_unrecoverable);
        assert!(!setup_body.setup_retryable);

        let response = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: None,
                state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
        let manifest = build_manifest_json("https://djinn.example.com", "a1b2c3d4");
        // App name prefix is `djinn-`
        assert_eq!(manifest["name"], "djinn-djinn.example.com-a1b2c3d4");
        assert_eq!(manifest["url"], "https://djinn.example.com");
        assert_eq!(
            manifest["redirect_url"],
            "https://djinn.example.com/auth/github/app-manifest-callback"
        );
        assert_eq!(
            manifest["callback_urls"][0],
            "https://djinn.example.com/auth/github/callback"
        );
        assert!(
            manifest.get("hook_attributes").is_none(),
            "webhooks are disabled, so the manifest must omit hook_attributes entirely"
        );
        assert_eq!(manifest["request_oauth_on_install"], true);
        assert_eq!(manifest["public"], false);
        // Permissions match current runtime consumers. `pull_requests:write`
        // also authorizes PR issue comments, so a separate `issues` grant is
        // unnecessary.
        // (metadata:read is granted by GitHub automatically, not listed).
        assert_eq!(manifest["default_permissions"]["actions"], "read");
        assert_eq!(manifest["default_permissions"]["checks"], "read");
        assert_eq!(manifest["default_permissions"]["contents"], "write");
        assert_eq!(manifest["default_permissions"]["members"], "read");
        assert_eq!(manifest["default_permissions"]["pull_requests"], "write");
        assert!(manifest["default_permissions"].get("metadata").is_none());
        // No extra permissions beyond the required set.
        let perms = manifest["default_permissions"].as_object().unwrap();
        assert_eq!(
            perms.len(),
            5,
            "only actions, checks, contents, members, pull_requests"
        );
        // Round-trips as valid JSON.
        let s = manifest.to_string();
        let _back: serde_json::Value = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn manifest_json_name_uses_djinn_prefix() {
        let manifest = build_manifest_json("https://mycompany.example.com", "retry123");
        let name = manifest["name"].as_str().unwrap();
        assert!(
            name.starts_with("djinn-"),
            "name must start with djinn-, got: {name}"
        );
    }

    #[test]
    fn manifest_json_no_extra_events() {
        let manifest = build_manifest_json("https://djinn.example.com", "a1b2c3d4");
        // No default_events field — the design requires only permissions.
        assert!(
            manifest.get("default_events").is_none(),
            "manifest should not include default_events"
        );
        assert!(
            manifest.get("hook_attributes").is_none(),
            "manifest should not include webhook configuration"
        );
    }

    #[test]
    fn local_manifest_does_not_submit_an_invalid_localhost_webhook_url() {
        let manifest = build_manifest_json("http://localhost:8372", "local123");
        let serialized = manifest.to_string();

        assert!(manifest.get("hook_attributes").is_none());
        assert!(!serialized.contains("/webhooks/github"));
        assert_eq!(
            manifest["redirect_url"],
            "http://localhost:8372/auth/github/app-manifest-callback"
        );
        assert_eq!(
            manifest["callback_urls"][0],
            "http://localhost:8372/auth/github/callback"
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
    fn manifest_name_suffix_is_fresh_and_token_independent() {
        let first = manifest_name_suffix();
        let second = manifest_name_suffix();
        assert_eq!(first.len(), 8);
        assert_eq!(second.len(), 8);
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(second.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(first, second);
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unusable_post_persist_resolution_fails_and_preserves_setup_session() {
        use crate::test_helpers;
        use djinn_provider::github_app::InvalidSecretDetail;

        let _lock = with_self_setup_override(Some(true)).await;
        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 4242,
            slug: "djinn-runtime".into(),
            client_id: "Iv1.runtime".into(),
            client_secret: "runtime-secret".into(),
            pem: "PEM".into(),
            webhook_secret: None,
        }));

        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_test_reload_state_override(Some(CredentialSourceState::InvalidSecret(
                InvalidSecretDetail {
                    issues: vec!["GITHUB_APP_ID is empty"],
                },
            )))
            .await;
        let session = register_valid_session(&state).await;
        let csrf = "persist-failure-csrf";
        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let response = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("valid-code".into()),
                state: Some(csrf.into()),
            }),
            headers,
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(state.validate_setup_session_token(&session).await);
        assert!(matches!(
            state.app_credential_state().await,
            CredentialSourceState::InvalidSecret(_)
        ));
        assert!(state.app_config().await.is_none());

        state.set_test_reload_state_override(None).await;
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
        // Install-continuation nonce must be present in the redirect.
        assert!(
            location.contains(INSTALL_CONTINUATION_PARAM),
            "install URL must contain continuation param, got: {location}"
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
        let runtime_cfg = djinn_provider::github_app::runtime_config()
            .expect("provider runtime cache must be hot-reloaded with persisted credentials");
        assert_eq!(runtime_cfg.app_id, 42);
        assert_eq!(runtime_cfg.slug, "djinn-test");
        assert_eq!(
            djinn_provider::github_app::bot_git_identity().0,
            "djinn-test[bot]"
        );

        assert!(
            !state.validate_setup_session_token(&session).await,
            "old setup session token must be invalidated after successful credential persistence"
        );

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

    // ─── Install-continuation flow tests (AC1) ───────────────────────────

    /// `/auth/github/callback` with `installation_id` and `setup_action=install`
    /// redirects to `/auth/github/app-setup-callback` preserving the
    /// installation params. This is the install-continuation entry point that
    /// GitHub uses when `request_oauth_on_install: true` in the manifest.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_callback_forwards_install_to_app_setup_callback() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        let headers = HeaderMap::new();

        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: None,
                state: None,
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
            }),
            headers,
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::FOUND,
            "must redirect to app-setup-callback"
        );
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            location.contains("/auth/github/app-setup-callback"),
            "must redirect to app-setup-callback, got: {location}"
        );
        assert!(
            location.contains("installation_id=42"),
            "must preserve installation_id, got: {location}"
        );
        assert!(
            location.contains("setup_action=install"),
            "must preserve setup_action, got: {location}"
        );
    }

    /// The secured code-only install redirect carries only the dedicated
    /// install-continuation capability; it never reflects the GitHub OAuth
    /// code, normal OAuth state, boot token, or Djinn session token.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_redirect_no_token_leak() {
        use crate::test_helpers;
        use djinn_provider::github_app::Installation;

        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_config(Some(Arc::new(djinn_provider::github_app::AppConfig {
                app_id: 42,
                slug: "djinn-no-leak-test".into(),
                client_id: "Iv1.no-leak-test".into(),
                client_secret: "no-leak-secret".into(),
                pem: "PEM".into(),
                webhook_secret: String::new(),
                public_url: "http://127.0.0.1:8372".into(),
            })))
            .await;

        let continuation = "no-leak-install-continuation";
        state
            .set_pending_install_continuation_for_tests(Some(continuation.into()))
            .await;
        *OAUTH_EXCHANGE_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(GithubUserTokens {
            access_token: "ghu_no_leak_install_user".into(),
            expires_in: Some(28_800),
            refresh_token: Some("ghr_no_leak_install_user".into()),
            refresh_token_expires_in: Some(15_897_600),
        }));
        *USER_INSTALLATIONS_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(vec![Installation {
            id: 99,
            account_login: "acme".into(),
            account_type: "Organization".into(),
            target_type: "Organization".into(),
        }]));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{INSTALL_CONTINUATION_COOKIE}={continuation}"))
                .unwrap(),
        );

        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: Some("leaked-code".into()),
                state: None,
                installation_id: None,
                setup_action: None,
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !location.contains("code="),
            "OAuth code must not leak: {location}"
        );
        assert!(
            !location.contains("state="),
            "OAuth state must not leak: {location}"
        );
        assert!(
            !location.contains("setup_token"),
            "setup_token must not leak: {location}"
        );
    }

    /// When `installation_id` is present but `setup_action` is not `install`,
    /// the callback falls through to the normal OAuth flow (not the install
    /// redirect). This preserves the existing callback behavior for
    /// `setup_action=update` or other values.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_callback_non_install_action_falls_through_to_oauth() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: None,
                state: None,
                installation_id: Some("42".into()),
                setup_action: Some("update".into()),
            }),
            HeaderMap::new(),
        )
        .await;

        // Without code/state, it should return BAD_REQUEST (falling through
        // to the normal OAuth validation), not a redirect to app-setup-callback.
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "non-install setup_action should fall through to OAuth validation"
        );
    }

    // ─── Regression: production auth behavior (AC4) ──────────────────────

    /// With `DJINN_ENABLE_SELF_SETUP` unset/false, ALL setup routes return
    /// 404 and `/auth/config` does not advertise setup availability.
    /// This is the production default — no setup affordances leak.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn self_setup_disabled_all_setup_routes_hidden_and_config_clean() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(false)).await;
        let state = test_helpers::test_app_state_in_memory().await;

        // Config must not advertise self-setup.
        let cfg_resp = config(State(state.clone()), HeaderMap::new()).await;
        assert!(
            !cfg_resp.0.self_setup_available,
            "self_setup_available must be false when gate is disabled"
        );

        // create-app returns 404.
        let resp = create_app(
            State(state.clone()),
            Query(CreateAppQuery { setup_token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // manifest-callback returns 404.
        let resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: None,
                state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// With configured Secret/env credentials (app_config set) and the
    /// self-setup gate enabled, setup routes still return 404 because usable
    /// credentials take precedence. This is the "production configured" path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_credentials_hide_setup_routes_and_config_reports_configured() {
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

        // Config must report configured=true and self_setup_available=false.
        let cfg_resp = config(State(state.clone()), HeaderMap::new()).await;
        assert!(cfg_resp.0.configured, "must report configured=true");
        assert!(
            !cfg_resp.0.self_setup_available,
            "must not advertise self-setup when credentials exist"
        );
        assert!(
            cfg_resp.0.missing.is_empty(),
            "no missing env vars when credentials are loaded"
        );

        // Setup routes return 404 even with gate enabled.
        let resp = create_app(
            State(state.clone()),
            Query(CreateAppQuery { setup_token: None }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: None,
                state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// `/auth/github/start` with configured credentials produces a 302
    /// redirect to GitHub's OAuth authorize page with the expected params.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_start_produces_oauth_redirect_with_credentials() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let cfg = djinn_provider::github_app::AppConfig {
            app_id: 1,
            slug: "djinn".into(),
            client_id: "Iv1.test-client-id".into(),
            client_secret: "secret".into(),
            pem: "PEM".into(),
            webhook_secret: "w".into(),
            public_url: "http://127.0.0.1:8372".into(),
        };
        state.set_app_config(Some(Arc::new(cfg))).await;

        let resp = github_start(
            State(state),
            Query(StartQuery {
                redirect: Some("/tasks".into()),
                install: None,
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            location.starts_with("https://github.com/login/oauth/authorize"),
            "must redirect to GitHub OAuth, got: {location}"
        );
        assert!(
            location.contains("client_id=Iv1.test-client-id"),
            "must include client_id, got: {location}"
        );
        assert!(
            location.contains("redirect_uri="),
            "must include redirect_uri, got: {location}"
        );
        assert!(
            location.contains("state="),
            "must include CSRF state, got: {location}"
        );

        // OAuth state cookie must be set.
        let set_cookies: Vec<_> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap_or("").to_string())
            .collect();
        assert!(
            set_cookies
                .iter()
                .any(|c| c.starts_with(OAUTH_STATE_COOKIE)),
            "OAuth state cookie must be set: {set_cookies:?}"
        );
    }

    /// `/auth/github/start` without configured credentials returns 503.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_start_returns_503_without_credentials() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let resp = github_start(
            State(state),
            Query(StartQuery {
                redirect: None,
                install: None,
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// `app-setup-callback` rejects missing or invalid installation_id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_setup_callback_rejects_missing_or_invalid_installation_id() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        // Missing installation_id.
        let resp = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: None,
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Empty installation_id.
        let resp = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some(String::new()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Non-numeric installation_id.
        let resp = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some("not-a-number".into()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Zero installation_id.
        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("0".into()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `app-setup-callback` returns CONFLICT when no credentials are
    /// configured. This is the case when GitHub redirects here but the
    /// deployment hasn't set up the App yet.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_setup_callback_returns_conflict_without_credentials() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        let continuation = "missing-credentials-continuation";
        state
            .set_pending_install_continuation_for_tests(Some(continuation.into()))
            .await;

        let resp = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
                continuation_state: Some(continuation.into()),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::CONFLICT,
            "must return 409 when credentials are missing"
        );
        assert!(
            state
                .validate_pending_install_continuation(continuation)
                .await,
            "credential recovery must preserve the valid continuation for retry"
        );
    }

    // ─── Install-continuation state validation (AC1/AC4) ────────────────

    /// When a pending install-continuation nonce exists and the callback
    /// carries the matching `continuation_state` param, the callback proceeds
    /// normally and the nonce is consumed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_setup_callback_valid_continuation_state_succeeds() {
        use crate::test_helpers;
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

        // Set a pending continuation nonce.
        let nonce = "test-continuation-nonce-abc".to_string();
        state
            .set_pending_install_continuation_for_tests(Some(nonce.clone()))
            .await;

        // The callback carries the matching continuation_state — but will
        // still fail because fetch_installation_for_setup requires a real
        // GitHub App JWT. We just verify the continuation validation passes
        // (status != FORBIDDEN).
        let resp = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
                continuation_state: Some(nonce.clone()),
            }),
            HeaderMap::new(),
        )
        .await;

        // Should NOT be FORBIDDEN — continuation passed.
        // It will fail at the JWT/installation step (BAD_GATEWAY or similar),
        // which proves the continuation check didn't block it.
        assert_ne!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "valid continuation must not be rejected"
        );

        // The downstream GitHub call failed, so the nonce must remain valid
        // for a retry instead of being consumed by validation alone.
        assert!(
            state.validate_pending_install_continuation(&nonce).await,
            "fallible installation lookup must preserve the continuation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_install_binding_consumes_nonce_then_starts_stateful_oauth() {
        use crate::test_helpers;
        use djinn_provider::github_server::InstallationAccount;

        let _lock = with_self_setup_override(Some(true)).await;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_config(Some(Arc::new(djinn_provider::github_app::AppConfig {
                app_id: 42,
                slug: "djinn-install-test".into(),
                client_id: "Iv1.install-test".into(),
                client_secret: "install-secret".into(),
                pem: "PEM".into(),
                webhook_secret: String::new(),
                public_url: "http://127.0.0.1:8372".into(),
            })))
            .await;
        let continuation = "binding-continuation";
        state
            .set_pending_install_continuation_for_tests(Some(continuation.into()))
            .await;
        *APP_INSTALLATION_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(AppInstallation {
            id: 777,
            account: Some(InstallationAccount {
                id: 9001,
                login: "acme".into(),
                account_type: "Organization".into(),
            }),
            repository_selection: Some("all".into()),
            html_url: None,
        }));

        let response = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some("777".into()),
                setup_action: Some("install".into()),
                continuation_state: Some(continuation.into()),
            }),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "/auth/github/start?redirect=%2F"
        );
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(|cookie| {
                    cookie.starts_with(&format!("{INSTALL_CONTINUATION_COOKIE}="))
                        && cookie.contains("Max-Age=0")
                }),
            "successful binding must clear the continuation cookie"
        );
        assert!(!state.has_pending_install_continuation().await);
        let binding = OrgConfigRepository::new(state.db().clone())
            .get()
            .await
            .unwrap()
            .expect("binding must be persisted before stateful sign-in");
        assert_eq!(binding.installation_id, 777);
        assert_eq!(binding.github_org_login, "acme");

        // A later setup callback for a different installation must not
        // replace the deployment's established one-org binding.
        let conflicting_continuation = "conflicting-binding-continuation";
        state
            .set_pending_install_continuation_for_tests(Some(conflicting_continuation.into()))
            .await;
        *APP_INSTALLATION_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(AppInstallation {
            id: 888,
            account: Some(InstallationAccount {
                id: 9002,
                login: "other-org".into(),
                account_type: "Organization".into(),
            }),
            repository_selection: Some("all".into()),
            html_url: None,
        }));

        let conflict = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some("888".into()),
                setup_action: Some("install".into()),
                continuation_state: Some(conflicting_continuation.into()),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let unchanged = OrgConfigRepository::new(state.db().clone())
            .get()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.installation_id, 777);
        assert_eq!(unchanged.github_org_login, "acme");
        assert!(
            state
                .validate_pending_install_continuation(conflicting_continuation)
                .await,
            "a rejected rebind must not consume the retry/audit nonce"
        );

        *APP_INSTALLATION_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    /// When a pending continuation nonce exists but the callback does NOT
    /// carry a `continuation_state` param, the callback is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_setup_callback_missing_continuation_state_rejected() {
        use crate::test_helpers;
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

        // Set a pending continuation nonce.
        state
            .set_pending_install_continuation_for_tests(Some("expected-nonce".into()))
            .await;

        // Callback without continuation_state — should be rejected.
        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "missing continuation_state must be rejected when one is pending"
        );
    }

    /// When a pending continuation nonce exists but the callback carries a
    /// wrong `continuation_state`, the callback is rejected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_setup_callback_mismatched_continuation_state_rejected() {
        use crate::test_helpers;
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

        // Set a pending continuation nonce.
        state
            .set_pending_install_continuation_for_tests(Some("correct-nonce".into()))
            .await;

        // Callback with a different continuation_state — should be rejected.
        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
                continuation_state: Some("wrong-nonce".into()),
            }),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "mismatched continuation_state must be rejected"
        );
    }

    /// An uncorrelated callback must not create the deployment's first binding,
    /// even when App credentials were pre-configured.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_setup_callback_no_pending_continuation_rejects_first_binding() {
        use crate::test_helpers;
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

        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "no pending continuation must not authorize the first binding"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_setup_callback_allows_uncorrelated_exact_idempotent_replay() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;
        state
            .set_app_config(Some(Arc::new(djinn_provider::github_app::AppConfig {
                app_id: 1,
                slug: "djinn".into(),
                client_id: "Iv1.x".into(),
                client_secret: "y".into(),
                pem: "PEM".into(),
                webhook_secret: "w".into(),
                public_url: "http://127.0.0.1:8372".into(),
            })))
            .await;
        OrgConfigRepository::new(state.db().clone())
            .set(NewOrgConfig {
                github_org_id: 7,
                github_org_login: "acme",
                app_id: 1,
                installation_id: 42,
            })
            .await
            .unwrap();

        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;

        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// The continuation nonce is included in the manifest callback's install
    /// URL redirect and can be used to validate the subsequent
    /// app-setup-callback request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_redirect_contains_valid_continuation_for_setup_callback() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        state.set_test_bypass_persist(true).await;
        let csrf = "csrf-continuation-roundtrip".to_string();

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "test-secret".into(),
            webhook_secret: Some("test-webhook-secret".into()),
            pem: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
        }));

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("valid-code".into()),
                state: Some(csrf),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Extract the continuation nonce from the URL.
        let continuation_nonce = location
            .split(&format!("{INSTALL_CONTINUATION_PARAM}="))
            .nth(1)
            .expect("continuation param must be present")
            .split('&')
            .next()
            .unwrap();
        assert!(
            !continuation_nonce.is_empty(),
            "continuation nonce must not be empty"
        );

        // Now simulate the app-setup-callback with the extracted nonce.
        // It should NOT be rejected with FORBIDDEN (the nonce matches).
        let resp = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
                continuation_state: Some(continuation_nonce.to_string()),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_ne!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "matching continuation nonce must not be rejected"
        );

        // The authoritative installation lookup failed, so the nonce remains
        // pending and a retry without it must still be rejected.
        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "failed binding must preserve and continue requiring the nonce"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    // ─── Manifest continuation through the /auth/github/callback bridge ──────
    //
    // These tests verify the end-to-end manifest continuation flow:
    //   1. `app_manifest_callback` persists credentials and sets a continuation
    //      nonce both in the install URL and in the `djinn_install_continuation`
    //      cookie.
    //   2. GitHub redirects to `/auth/github/callback?installation_id=...&setup_action=install`
    //      (dropping any custom query params). The browser sends the cookie.
    //   3. `github_callback` reads the cookie, appends it as `djinn_continuation`
    //      to the `app-setup-callback` redirect URL.
    //   4. `app_setup_callback` validates the continuation nonce.

    /// Helper: extract the continuation cookie value from a response's
    /// `Set-Cookie` headers.
    fn extract_set_cookie_value(resp: &Response, cookie_name: &str) -> Option<String> {
        for hv in resp.headers().get_all(header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            if s.starts_with(&format!("{cookie_name}=")) {
                // Value is between "name=" and the first ";"
                return Some(
                    s[cookie_name.len() + 1..]
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .to_string(),
                );
            }
        }
        None
    }

    /// The manifest callback sets the `djinn_install_continuation` cookie so the
    /// nonce survives the cross-domain GitHub install round-trip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_sets_continuation_cookie() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        state.set_test_bypass_persist(true).await;
        let csrf = "csrf-cookie-test".to_string();

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "test-secret".into(),
            webhook_secret: Some("test-webhook-secret".into()),
            pem: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
        }));

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("valid-code".into()),
                state: Some(csrf),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FOUND);

        // The continuation cookie must be present in Set-Cookie.
        let cookie_val = extract_set_cookie_value(&resp, INSTALL_CONTINUATION_COOKIE)
            .expect("install-continuation cookie must be set");
        assert!(!cookie_val.is_empty(), "cookie value must not be empty");

        // The cookie value must match the nonce in the install URL.
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        let url_nonce = location
            .split(&format!("{INSTALL_CONTINUATION_PARAM}="))
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap();
        assert_eq!(
            cookie_val, url_nonce,
            "cookie value must match the URL nonce"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    /// Full bridge: manifest success → `/auth/github/callback` (with
    /// continuation cookie) → redirect to `app-setup-callback` carries the
    /// continuation param → `app_setup_callback` validates it.
    ///
    /// This is the core regression test for AC1/AC4: the continuation state
    /// must survive through the real callback bridge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_continuation_through_callback_bridge_valid() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        state.set_test_bypass_persist(true).await;
        let csrf = "csrf-bridge-valid".to_string();

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "test-secret".into(),
            webhook_secret: Some("test-webhook-secret".into()),
            pem: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
        }));

        // Step 1: Manifest callback — persist credentials, get install URL
        // with continuation nonce, and the continuation cookie.
        let mut manifest_headers = headers_with_session(&session);
        manifest_headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let manifest_resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("valid-code".into()),
                state: Some(csrf),
            }),
            manifest_headers,
        )
        .await;
        assert_eq!(manifest_resp.status(), StatusCode::FOUND);

        // Extract the continuation cookie value from the manifest response.
        let continuation_cookie_val =
            extract_set_cookie_value(&manifest_resp, INSTALL_CONTINUATION_COOKIE)
                .expect("continuation cookie must be set by manifest callback");

        // Step 2: Simulate GitHub's redirect to /auth/github/callback after
        // the install. GitHub drops custom query params, but the browser sends
        // the continuation cookie.
        let mut callback_headers = HeaderMap::new();
        callback_headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!(
                "{INSTALL_CONTINUATION_COOKIE}={continuation_cookie_val}"
            ))
            .unwrap(),
        );

        let callback_resp = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: None,
                state: None,
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
            }),
            callback_headers,
        )
        .await;

        // Must redirect to app-setup-callback with the continuation param.
        assert_eq!(callback_resp.status(), StatusCode::FOUND);
        let callback_location = callback_resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            callback_location.contains("/auth/github/app-setup-callback"),
            "must redirect to app-setup-callback, got: {callback_location}"
        );
        assert!(
            callback_location.contains(&format!(
                "{INSTALL_CONTINUATION_PARAM}={continuation_cookie_val}"
            )),
            "redirect must carry the continuation nonce, got: {callback_location}"
        );

        // Step 3: Simulate app-setup-callback with the continuation param
        // from the redirect URL. The nonce matches, so it must NOT be
        // rejected with FORBIDDEN.
        let resp = app_setup_callback(
            State(state.clone()),
            Query(AppSetupQuery {
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
                continuation_state: Some(continuation_cookie_val),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_ne!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "valid continuation through the callback bridge must not be rejected"
        );
        // The fake App key makes the authoritative installation lookup fail;
        // validation alone must not consume the retry nonce.
        assert!(
            state.has_pending_install_continuation().await,
            "nonce must remain pending until binding or picker transition succeeds"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    /// Bridge with MISSING continuation: GitHub redirects to
    /// `/auth/github/callback` without the continuation cookie (e.g. the
    /// browser blocked it). The redirect to `app-setup-callback` will NOT
    /// carry `djinn_continuation`, and `app_setup_callback` must reject it
    /// with FORBIDDEN because a pending nonce exists.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_continuation_through_callback_bridge_missing_rejected() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        state.set_test_bypass_persist(true).await;
        let csrf = "csrf-bridge-missing".to_string();

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "test-secret".into(),
            webhook_secret: Some("test-webhook-secret".into()),
            pem: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
        }));

        // Step 1: Manifest callback — persist credentials, set continuation.
        let mut manifest_headers = headers_with_session(&session);
        manifest_headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let manifest_resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("valid-code".into()),
                state: Some(csrf),
            }),
            manifest_headers,
        )
        .await;
        assert_eq!(manifest_resp.status(), StatusCode::FOUND);

        // Step 2: GitHub callback WITHOUT the continuation cookie (simulating
        // a browser that dropped/blocked it).
        let callback_resp = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: None,
                state: None,
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
            }),
            HeaderMap::new(), // no continuation cookie
        )
        .await;

        assert_eq!(callback_resp.status(), StatusCode::FOUND);
        let callback_location = callback_resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();

        // The redirect must NOT carry the continuation param.
        assert!(
            !callback_location.contains(INSTALL_CONTINUATION_PARAM),
            "redirect must not carry continuation when cookie is absent, got: {callback_location}"
        );

        // Step 3: app-setup-callback without continuation — must be FORBIDDEN.
        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
                continuation_state: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "missing continuation through the callback bridge must be rejected"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    /// Bridge with MISMATCHED continuation: the cookie carries a different
    /// nonce than what was stored. `app-setup-callback` must reject it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_continuation_through_callback_bridge_mismatched_rejected() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        state.set_test_bypass_persist(true).await;
        let csrf = "csrf-bridge-mismatch".to_string();

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "test-secret".into(),
            webhook_secret: Some("test-webhook-secret".into()),
            pem: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
        }));

        // Step 1: Manifest callback — persist credentials, set continuation.
        let mut manifest_headers = headers_with_session(&session);
        manifest_headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let manifest_resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("valid-code".into()),
                state: Some(csrf),
            }),
            manifest_headers,
        )
        .await;
        assert_eq!(manifest_resp.status(), StatusCode::FOUND);

        // Step 2: GitHub callback with a WRONG continuation cookie value.
        let mut callback_headers = HeaderMap::new();
        callback_headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{INSTALL_CONTINUATION_COOKIE}=wrong-nonce-value"))
                .unwrap(),
        );

        let callback_resp = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: None,
                state: None,
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
            }),
            callback_headers,
        )
        .await;

        assert_eq!(callback_resp.status(), StatusCode::FOUND);
        let callback_location = callback_resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Extract the (wrong) continuation from the redirect URL.
        let wrong_nonce = callback_location
            .split(&format!("{INSTALL_CONTINUATION_PARAM}="))
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap_or("wrong-nonce-value");

        // Step 3: app-setup-callback with the wrong continuation — must be FORBIDDEN.
        let resp = app_setup_callback(
            State(state),
            Query(AppSetupQuery {
                installation_id: Some("99".into()),
                setup_action: Some("install".into()),
                continuation_state: Some(wrong_nonce.to_string()),
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "mismatched continuation through the callback bridge must be rejected"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    /// The `github_callback` install redirect carries the continuation cookie
    /// value as a query param when the cookie is present.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_callback_install_redirect_carries_continuation_from_cookie() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!(
                "{INSTALL_CONTINUATION_COOKIE}=test-nonce-from-cookie"
            ))
            .unwrap(),
        );

        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: None,
                state: None,
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            location.contains(&format!(
                "{INSTALL_CONTINUATION_PARAM}=test-nonce-from-cookie"
            )),
            "redirect must carry continuation from cookie, got: {location}"
        );
    }

    /// The `github_callback` install redirect does NOT carry a continuation
    /// param when no continuation cookie is present (non-manifest flow).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_callback_install_redirect_no_continuation_without_cookie() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: None,
                state: None,
                installation_id: Some("42".into()),
                setup_action: Some("install".into()),
            }),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !location.contains(INSTALL_CONTINUATION_PARAM),
            "redirect must not carry continuation without cookie, got: {location}"
        );
    }

    /// `/auth/github/callback` without the OAuth state cookie returns 400.
    /// This is a regression guard ensuring the CSRF state check is not
    /// bypassed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_callback_rejects_missing_state_cookie() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: Some("some-code".into()),
                state: Some("some-state".into()),
                installation_id: None,
                setup_action: None,
            }),
            HeaderMap::new(),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `/auth/github/callback` with mismatched state returns 400.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_callback_rejects_state_mismatch() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{}=cookie-state", OAUTH_STATE_COOKIE)).unwrap(),
        );

        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: Some("code".into()),
                state: Some("different-state".into()),
                installation_id: None,
                setup_action: None,
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// `/auth/github/callback` with missing code or state returns 400.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn github_callback_rejects_missing_code_or_state() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        // Missing code.
        let resp = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: None,
                state: Some("state".into()),
                installation_id: None,
                setup_action: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Missing state.
        let resp = github_callback(
            State(state.clone()),
            Query(CallbackQuery {
                code: Some("code".into()),
                state: None,
                installation_id: None,
                setup_action: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Empty code.
        let resp = github_callback(
            State(state),
            Query(CallbackQuery {
                code: Some(String::new()),
                state: Some("state".into()),
                installation_id: None,
                setup_action: None,
            }),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ─── No-token-leak assertions (AC5) ──────────────────────────────────

    /// OAuth start redirect URL contains only the expected params — no
    /// boot token, setup session, or credential material.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oauth_start_redirect_no_token_leak() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let cfg = djinn_provider::github_app::AppConfig {
            app_id: 1,
            slug: "djinn".into(),
            client_id: "Iv1.test-client-id".into(),
            client_secret: "super-secret-value".into(),
            pem: "SENSITIVE-PEM".into(),
            webhook_secret: "webhook-secret".into(),
            public_url: "http://127.0.0.1:8372".into(),
        };
        state.set_app_config(Some(Arc::new(cfg))).await;

        // Inject a boot token and setup session — neither should appear.
        let (raw_token, bt) = boot_token::BootToken::generate();
        state.set_boot_token_for_tests(Some(bt)).await;

        let resp = github_start(
            State(state),
            Query(StartQuery {
                redirect: None,
                install: None,
            }),
        )
        .await;

        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !location.contains("setup_token"),
            "setup_token must not appear: {location}"
        );
        assert!(
            !location.contains(&raw_token),
            "raw boot token must not appear: {location}"
        );
        assert!(
            !location.contains("super-secret-value"),
            "client_secret must not appear: {location}"
        );
        assert!(
            !location.contains("SENSITIVE-PEM"),
            "private key must not appear: {location}"
        );
    }

    /// After successful manifest callback, the install URL redirect does
    /// not contain any credential material or setup tokens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_callback_install_redirect_no_token_leak() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        state.set_test_bypass_persist(true).await;
        let csrf = "csrf-no-leak-test".to_string();

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "leaked-secret".into(),
            webhook_secret: Some("leaked-webhook".into()),
            pem: "SENSITIVE-PEM-KEY".into(),
        }));

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        let resp = app_manifest_callback(
            State(state),
            Query(ManifestCallbackQuery {
                code: Some("manifest-code".into()),
                state: Some(csrf),
            }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        // Must be a clean GitHub install URL.
        assert!(
            location.contains("github.com/apps/"),
            "must redirect to GitHub, got: {location}"
        );
        // Install-continuation nonce must be present (it's a public nonce,
        // not a secret — same class as CSRF state).
        assert!(
            location.contains(INSTALL_CONTINUATION_PARAM),
            "install URL must contain continuation param, got: {location}"
        );
        // No credential or token leaks.
        assert!(
            !location.contains("leaked-secret"),
            "client_secret must not leak: {location}"
        );
        assert!(
            !location.contains("leaked-webhook"),
            "webhook_secret must not leak: {location}"
        );
        assert!(
            !location.contains("SENSITIVE-PEM"),
            "PEM must not leak: {location}"
        );
        assert!(
            !location.contains("setup_token"),
            "setup_token must not leak: {location}"
        );
        assert!(
            !location.contains("manifest-code"),
            "manifest code must not leak: {location}"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    /// The CSRF state value in the OAuth start redirect is a public nonce,
    /// not a secret. Verify it is base64-encoded random bytes (same format
    /// as our random tokens) and does not contain raw credential content.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oauth_state_is_public_csrf_nonce_not_secret() {
        use crate::test_helpers;
        let state = test_helpers::test_app_state_in_memory().await;

        let cfg = djinn_provider::github_app::AppConfig {
            app_id: 1,
            slug: "djinn".into(),
            client_id: "Iv1.x".into(),
            client_secret: "secret".into(),
            pem: "PEM".into(),
            webhook_secret: "w".into(),
            public_url: "http://127.0.0.1:8372".into(),
        };
        state.set_app_config(Some(Arc::new(cfg))).await;

        let resp = github_start(
            State(state),
            Query(StartQuery {
                redirect: None,
                install: None,
            }),
        )
        .await;

        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();

        // Extract the state param.
        let state_param = location
            .split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        // Must be URL-safe base64 (our random_token_b64 format).
        assert!(
            !state_param.contains('+') && !state_param.contains('/') && !state_param.contains('='),
            "state must be URL-safe base64: {state_param}"
        );
        assert_eq!(state_param.len(), 43, "32 bytes → 43 base64 chars");
    }

    // ─── Config reporting after persistence (AC3) ────────────────────────

    /// After successful manifest callback with credential persistence,
    /// `/auth/config` reports `configured=true`, `self_setup_available=false`,
    /// and no missing env vars (credentials came from manifest, not env).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn config_reports_configured_and_no_setup_after_manifest_persistence() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        state.set_test_bypass_persist(true).await;
        let csrf = "csrf-config-test".to_string();

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Ok(ManifestConversion {
            id: 42,
            slug: "djinn-test".into(),
            client_id: "Iv1.test-client".into(),
            client_secret: "test-secret".into(),
            webhook_secret: None,
            pem: "-----BEGIN RSA PRIVATE KEY-----\ntest\n-----END RSA PRIVATE KEY-----".into(),
        }));

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );

        // Run the manifest callback.
        let resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("code".into()),
                state: Some(csrf),
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FOUND);

        // Now check /auth/config.
        let cfg_resp = config(State(state), HeaderMap::new()).await;
        assert!(
            cfg_resp.0.configured,
            "config must report configured=true after persistence"
        );
        assert!(
            !cfg_resp.0.self_setup_available,
            "self_setup_available must be false after credentials persist"
        );
        // Credentials came from manifest, not env — so env vars are "missing"
        // but that's fine because the persisted creds are loaded.
        // Actually, with test bypass, the config is loaded directly into
        // app_config. The missing list is populated only when
        // active.is_none(), so it should be empty.
        assert!(
            cfg_resp.0.missing.is_empty(),
            "no missing vars when credentials are loaded"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    // ─── Retry from create-app after exchange failure (AC2) ──────────────

    /// Full retry flow: manifest exchange fails → setup session preserved →
    /// user retries from create-app → new manifest form rendered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_retry_flow_after_exchange_failure() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;
        let csrf = "retry-csrf".to_string();

        // 1. Exchange fails.
        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = Some(Err("GitHub says no".into()));

        let mut headers = headers_with_session(&session);
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{MANIFEST_STATE_COOKIE}={csrf}")).unwrap(),
        );
        let resp = app_manifest_callback(
            State(state.clone()),
            Query(ManifestCallbackQuery {
                code: Some("bad-code".into()),
                state: Some(csrf),
            }),
            headers,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        // 2. Setup session is still valid — user can hit create-app again.
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
            "create-app must succeed after exchange failure"
        );

        // Verify the response is HTML (the manifest form).
        let body = resp.into_body();
        let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);
        assert!(
            html.contains("<form"),
            "must render the manifest form for retry"
        );
        assert!(
            html.contains("github.com/settings/apps/new"),
            "form must target GitHub's app creation page"
        );

        // 3. After credentials are loaded, retry is no longer possible.
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
            "setup routes must be hidden after credentials are loaded"
        );

        *EXCHANGE_MANIFEST_RESULT_OVERRIDE.lock().unwrap() = None;
    }

    // ─── Manifest form shape (AC1) ───────────────────────────────────────

    /// The create-app manifest form renders the expected auto-submit HTML
    /// with the manifest JSON and CSRF state embedded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_app_renders_manifest_form_with_correct_target() {
        use crate::test_helpers;
        let _lock = with_self_setup_override(Some(true)).await;

        let state = test_helpers::test_app_state_in_memory().await;
        let session = register_valid_session(&state).await;

        let headers = headers_with_session(&session);
        let resp = create_app(
            State(state),
            Query(CreateAppQuery { setup_token: None }),
            headers,
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"), "must be HTML, got: {ct}");

        // Manifest state cookie must be set for CSRF protection.
        let set_cookies: Vec<_> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap_or("").to_string())
            .collect();
        assert!(
            set_cookies
                .iter()
                .any(|c| c.starts_with(MANIFEST_STATE_COOKIE)),
            "manifest CSRF cookie must be set: {set_cookies:?}"
        );

        let body = resp.into_body();
        let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes);

        // Form must target GitHub's app creation endpoint.
        assert!(
            html.contains("action=\"https://github.com/settings/apps/new"),
            "form must target GitHub: {html}"
        );
        // Must contain a `state` param in the action URL (CSRF).
        assert!(
            html.contains("state="),
            "form action must include CSRF state: {html}"
        );
        // Must contain the manifest JSON hidden input.
        assert!(
            html.contains("name=\"manifest\""),
            "form must have manifest input: {html}"
        );
        // Auto-submit script must be present.
        assert!(html.contains(".submit()"), "form must auto-submit: {html}");
    }
}
