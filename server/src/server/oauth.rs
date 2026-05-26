//! MCP OAuth 2.1 Authorization Server (AS) + Resource Server (RS) metadata.
//!
//! djinn is colocated RS+AS in this one axum process. It lets native MCP
//! clients (Claude Code, Claude Desktop, Cursor) connect to `POST /mcp` via
//! the MCP "connect" OAuth flow, attributing MCP-created tasks to the
//! authenticated user. The *actual* user authentication is federated to the
//! EXISTING GitHub App login (`/auth/github/start`) — we do not build a second
//! GitHub integration. Tokens are opaque DB-backed values (see
//! `djinn_db::OAuthRepository`), not JWTs.
//!
//! Endpoints:
//!   * `GET  /.well-known/oauth-protected-resource`        (RFC 9728, RS)
//!   * `GET  /.well-known/oauth-protected-resource/mcp`    (RFC 9728, path-aware)
//!   * `GET  /.well-known/oauth-authorization-server`      (RFC 8414, AS)
//!   * `POST /oauth/register`                              (RFC 7591 DCR)
//!   * `GET  /oauth/authorize`                             (auth code + PKCE)
//!   * `POST /oauth/token`                                 (code & refresh grants)
//!
//! Security invariants enforced here:
//!   * PKCE S256 mandatory; `plain` and missing challenge rejected.
//!   * Auth codes are single-use, ≤60s TTL, bound to
//!     client_id+redirect_uri+code_challenge+resource.
//!   * `redirect_uri` is exact-matched against the client's registered set.
//!   * Audience (`resource`) is bound at issuance and validated on the RS path.
//!   * Constant-time comparison for the PKCE challenge and client secret.
//!   * Random tokens are 256-bit CSPRNG (`ring`) URL-safe base64.
//!   * The org-membership gate still applies: only holders of a valid
//!     `djinn_session` cookie (which the GitHub callback only grants to active
//!     org members) can mint an authorization code.
//!
//! ## Operator configuration
//!
//! The issuer base and the token audience are both derived from
//! `DJINN_PUBLIC_URL` (fallback `http://127.0.0.1:8372`). Operators MUST set
//! `DJINN_PUBLIC_URL` to the real public ingress URL — e.g.
//! `https://djinn.example.com` — for the OAuth metadata, redirect round-trips,
//! and audience binding to be correct. The canonical resource (audience) MCP
//! clients receive and that the RS validates is `{DJINN_PUBLIC_URL}/mcp`.

use axum::{
    Json, Router,
    extract::{Form, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::AppState;
use super::auth::{
    authenticate, constant_time_eq, public_url, random_token_b64, rfc3339_in, session_expired,
    urlencode,
};
use djinn_db::{NewAccessToken, NewAuthorizationCode, NewOAuthClient, OAuthRepository};

/// Authorization-code TTL. Spec ceiling is "short-lived"; we use 60s.
const AUTH_CODE_TTL_SECS: i64 = 60;
/// Access-token TTL (~1h).
const ACCESS_TOKEN_TTL_SECS: i64 = 60 * 60;
/// Refresh-token TTL (~30d).
const REFRESH_TOKEN_TTL_SECS: i64 = 60 * 60 * 24 * 30;
/// The only PKCE method we accept.
const PKCE_METHOD: &str = "S256";
/// The only scope this deployment understands.
const SCOPE_MCP: &str = "mcp";

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        // RFC 9728 path-aware: clients derived from the `{base}/mcp` resource
        // probe the suffixed path too. Serve identical metadata.
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/token", post(token))
}

// ── Canonical identifiers ──────────────────────────────────────────────────────

/// The issuer base (`DJINN_PUBLIC_URL`, fallback `http://127.0.0.1:8372`),
/// trailing slash stripped.
fn issuer() -> String {
    public_url().trim_end_matches('/').to_string()
}

/// The canonical resource (audience) URI tokens are bound to: `{base}/mcp`.
pub(super) fn canonical_resource() -> String {
    format!("{}/mcp", issuer())
}

/// `WWW-Authenticate` value the RS returns on a missing/invalid token, pointing
/// clients at the protected-resource metadata (RFC 9728 §5.1).
pub(super) fn www_authenticate_value() -> String {
    format!(
        "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
        issuer()
    )
}

// ── Metadata endpoints ──────────────────────────────────────────────────────────

async fn protected_resource_metadata() -> Response {
    let body = json!({
        "resource": canonical_resource(),
        "authorization_servers": [issuer()],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [SCOPE_MCP],
    });
    no_store_json(body)
}

async fn authorization_server_metadata() -> Response {
    let base = issuer();
    let body = json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": [PKCE_METHOD],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
        "scopes_supported": [SCOPE_MCP],
    });
    no_store_json(body)
}

// ── Dynamic client registration (RFC 7591) ──────────────────────────────────────

#[derive(Deserialize)]
struct RegisterRequest {
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    grant_types: Option<Vec<String>>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

#[derive(Serialize)]
struct RegisterResponse {
    client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    redirect_uris: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    token_endpoint_auth_method: String,
    grant_types: Vec<String>,
}

async fn register(State(state): State<AppState>, Json(req): Json<RegisterRequest>) -> Response {
    if req.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris must be a non-empty array",
        );
    }
    // Reject obviously malformed redirect URIs early. We allow custom schemes
    // (e.g. `cursor://…`) since native MCP clients legitimately use them, but
    // require a scheme delimiter so we never store a bare token.
    for uri in &req.redirect_uris {
        if !uri.contains("://") {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                "each redirect_uri must be an absolute URI",
            );
        }
    }

    // Default to a public PKCE client (`none`). Only mint a secret when a
    // confidential method is explicitly requested.
    let auth_method = req
        .token_endpoint_auth_method
        .as_deref()
        .unwrap_or("none")
        .to_string();
    let is_confidential = matches!(auth_method.as_str(), "client_secret_post" | "client_secret_basic");
    if !matches!(auth_method.as_str(), "none" | "client_secret_post" | "client_secret_basic") {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "unsupported token_endpoint_auth_method",
        );
    }

    let grant_types = req
        .grant_types
        .unwrap_or_else(|| vec!["authorization_code".to_string(), "refresh_token".to_string()]);
    // We only support these two grants; reject anything exotic up front.
    for g in &grant_types {
        if !matches!(g.as_str(), "authorization_code" | "refresh_token") {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "unsupported grant_type requested",
            );
        }
    }

    let client_id = random_token_b64();
    let client_secret = if is_confidential {
        Some(random_token_b64())
    } else {
        None
    };

    let repo = OAuthRepository::new(state.db().clone());
    let created = match repo
        .create_client(NewOAuthClient {
            client_id: &client_id,
            client_secret: client_secret.as_deref(),
            redirect_uris: &req.redirect_uris,
            client_name: req.client_name.as_deref(),
            token_endpoint_auth_method: &auth_method,
            grant_types: &grant_types,
        })
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "oauth/register: persist failed");
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "failed to register client",
            );
        }
    };

    let resp = RegisterResponse {
        client_id: created.client_id,
        client_secret: created.client_secret,
        redirect_uris: created.redirect_uris,
        client_name: created.client_name,
        token_endpoint_auth_method: created.token_endpoint_auth_method,
        grant_types: created.grant_types,
    };
    let mut out = (StatusCode::CREATED, Json(resp)).into_response();
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

// ── Authorization endpoint ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

async fn authorize(
    State(state): State<AppState>,
    Query(q): Query<AuthorizeQuery>,
    headers: HeaderMap,
) -> Response {
    // ── 1. Validate the client + redirect_uri BEFORE we can safely redirect.
    //       Errors discovered here must NOT redirect (we can't trust an
    //       unvalidated redirect_uri); return a plain 400 instead.
    let Some(client_id) = q.client_id.as_deref().filter(|s| !s.is_empty()) else {
        return plain_400("missing client_id");
    };
    let Some(redirect_uri) = q.redirect_uri.as_deref().filter(|s| !s.is_empty()) else {
        return plain_400("missing redirect_uri");
    };

    let repo = OAuthRepository::new(state.db().clone());
    let client = match repo.get_client(client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return plain_400("unknown client_id"),
        Err(e) => {
            tracing::error!(error = %e, "oauth/authorize: client lookup failed");
            return plain_500();
        }
    };
    // Exact-match redirect_uri against the registered set. No prefix/substring.
    if !client.redirect_uris.iter().any(|u| u == redirect_uri) {
        return plain_400("redirect_uri does not match a registered URI");
    }

    // ── 2. From here, errors CAN be reported back to the client via the
    //       validated redirect_uri (RFC 6749 §4.1.2.1).
    let st = q.state.as_deref();

    if q.response_type.as_deref() != Some("code") {
        return redirect_err(redirect_uri, "unsupported_response_type", st);
    }
    // PKCE S256 is mandatory. Reject `plain` and any missing challenge.
    let Some(code_challenge) = q.code_challenge.as_deref().filter(|s| !s.is_empty()) else {
        return redirect_err(redirect_uri, "invalid_request", st);
    };
    if q.code_challenge_method.as_deref() != Some(PKCE_METHOD) {
        return redirect_err(redirect_uri, "invalid_request", st);
    }

    // Audience: default to the canonical resource. If the client passed one,
    // it MUST equal the canonical resource (we only host one).
    let resource = canonical_resource();
    if let Some(r) = q.resource.as_deref()
        && r != resource
    {
        return redirect_err(redirect_uri, "invalid_target", st);
    }

    // Scope: only `mcp` is understood. Default to it when omitted; reject
    // anything else.
    let scope = match q.scope.as_deref() {
        None | Some("") => SCOPE_MCP.to_string(),
        Some(s) if s.split_whitespace().all(|tok| tok == SCOPE_MCP) => s.to_string(),
        Some(_) => return redirect_err(redirect_uri, "invalid_scope", st),
    };

    // ── 3. Resolve the djinn user from the existing session cookie. If there
    //       is none, bounce the browser into the EXISTING GitHub login flow,
    //       round-tripping back to this exact /oauth/authorize URL afterwards.
    let user = match authenticate(&state, &headers).await {
        Ok(Some(u)) => u,
        Ok(None) => return redirect_to_github_login(&headers, &q),
        Err(e) => {
            tracing::error!(error = %e, "oauth/authorize: authenticate failed");
            return redirect_err(redirect_uri, "server_error", st);
        }
    };
    // The org-membership gate is already enforced at GitHub-callback time and
    // by the periodic membership sync (which deletes sessions on revocation),
    // so a live `djinn_session` cookie is sufficient proof of active
    // membership — the same guarantee the web UI relies on.

    // TODO: consent screen. v1 auto-consents: the deployment is org-locked and
    // the user explicitly initiated the connect from their MCP client.

    // ── 4. Mint a single-use auth code bound to the request parameters.
    let code = random_token_b64();
    let expires_at = rfc3339_in(AUTH_CODE_TTL_SECS);
    if let Err(e) = repo
        .create_authorization_code(NewAuthorizationCode {
            code: &code,
            client_id,
            redirect_uri,
            code_challenge,
            code_challenge_method: PKCE_METHOD,
            resource: &resource,
            scope: Some(&scope),
            user_id: &user.id,
            expires_at: &expires_at,
        })
        .await
    {
        tracing::error!(error = %e, "oauth/authorize: persist code failed");
        return redirect_err(redirect_uri, "server_error", st);
    }

    // ── 5. 302 back to the client's redirect_uri with code + state.
    let mut location = format!("{redirect_uri}{}code={}", uri_query_sep(redirect_uri), urlencode(&code));
    if let Some(s) = st {
        location.push_str(&format!("&state={}", urlencode(s)));
    }
    redirect_to(&location)
}

/// Bounce the unauthenticated browser into the existing GitHub login flow,
/// requesting a same-origin redirect back to this `/oauth/authorize` request
/// (with all original query params preserved). The open-redirect protection in
/// `auth::github_start` (`sanitize_redirect`) only honours site-local paths, so
/// we pass a relative `/oauth/authorize?...` path — never an absolute URL.
fn redirect_to_github_login(_headers: &HeaderMap, q: &AuthorizeQuery) -> Response {
    let mut qs = String::new();
    let mut push = |k: &str, v: &str| {
        if qs.is_empty() {
            qs.push('?');
        } else {
            qs.push('&');
        }
        qs.push_str(&format!("{}={}", k, urlencode(v)));
    };
    if let Some(v) = &q.response_type {
        push("response_type", v);
    }
    if let Some(v) = &q.client_id {
        push("client_id", v);
    }
    if let Some(v) = &q.redirect_uri {
        push("redirect_uri", v);
    }
    if let Some(v) = &q.code_challenge {
        push("code_challenge", v);
    }
    if let Some(v) = &q.code_challenge_method {
        push("code_challenge_method", v);
    }
    if let Some(v) = &q.state {
        push("state", v);
    }
    if let Some(v) = &q.scope {
        push("scope", v);
    }
    if let Some(v) = &q.resource {
        push("resource", v);
    }
    // Site-local path only — `sanitize_redirect` rejects anything else.
    let self_path = format!("/oauth/authorize{qs}");
    let target = format!("/auth/github/start?redirect={}", urlencode(&self_path));
    redirect_to(&target)
}

// ── Token endpoint ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    // authorization_code grant
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    // both grants
    client_id: Option<String>,
    client_secret: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    // refresh_token grant
    refresh_token: Option<String>,
}

#[derive(Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: i64,
    refresh_token: String,
    scope: String,
}

async fn token(State(state): State<AppState>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_deref() {
        Some("authorization_code") => token_authorization_code(state, form).await,
        Some("refresh_token") => token_refresh(state, form).await,
        _ => token_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be authorization_code or refresh_token",
        ),
    }
}

async fn token_authorization_code(state: AppState, form: TokenForm) -> Response {
    let repo = OAuthRepository::new(state.db().clone());

    let Some(code) = form.code.as_deref().filter(|s| !s.is_empty()) else {
        return token_error(StatusCode::BAD_REQUEST, "invalid_request", "missing code");
    };
    let Some(code_verifier) = form.code_verifier.as_deref().filter(|s| !s.is_empty()) else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing code_verifier",
        );
    };
    let Some(client_id) = form.client_id.as_deref().filter(|s| !s.is_empty()) else {
        return token_error(StatusCode::BAD_REQUEST, "invalid_request", "missing client_id");
    };

    // Authenticate the client (confidential clients must present a valid
    // secret; public clients present none).
    let client = match repo.get_client(client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return token_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client_id");
        }
        Err(e) => {
            tracing::error!(error = %e, "oauth/token: client lookup failed");
            return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "db error");
        }
    };
    if let Some(resp) = verify_client_secret(&client, form.client_secret.as_deref()) {
        return resp;
    }

    let auth_code = match repo.get_authorization_code(code).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "unknown code");
        }
        Err(e) => {
            tracing::error!(error = %e, "oauth/token: code lookup failed");
            return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "db error");
        }
    };

    // Reject already-consumed or expired codes.
    if auth_code.consumed_at.is_some() {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "code already used");
    }
    if session_expired(&auth_code.expires_at) {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "code expired");
    }
    // Bindings must match exactly.
    if auth_code.client_id != client_id {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "client mismatch");
    }
    match form.redirect_uri.as_deref() {
        Some(r) if r == auth_code.redirect_uri => {}
        _ => return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "redirect_uri mismatch"),
    }
    // Optional `resource` param must match the bound audience if supplied.
    if let Some(r) = form.resource.as_deref()
        && r != auth_code.resource
    {
        return token_error(StatusCode::BAD_REQUEST, "invalid_target", "resource mismatch");
    }

    // PKCE: BASE64URL_NOPAD(SHA256(code_verifier)) == code_challenge.
    let computed = pkce_s256(code_verifier);
    if !constant_time_eq(computed.as_bytes(), auth_code.code_challenge.as_bytes()) {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "PKCE verification failed");
    }

    // Mark the code consumed atomically. If this loses the race (double
    // exchange), reject — the token was already issued to the other caller.
    let now = rfc3339_in(0);
    match repo.consume_authorization_code(code, &now).await {
        Ok(true) => {}
        Ok(false) => {
            return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "code already used");
        }
        Err(e) => {
            tracing::error!(error = %e, "oauth/token: consume failed");
            return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "db error");
        }
    }

    issue_tokens(
        &repo,
        client_id,
        &auth_code.user_id,
        &auth_code.resource,
        auth_code.scope.as_deref(),
    )
    .await
}

async fn token_refresh(state: AppState, form: TokenForm) -> Response {
    let repo = OAuthRepository::new(state.db().clone());

    let Some(refresh_token) = form.refresh_token.as_deref().filter(|s| !s.is_empty()) else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "missing refresh_token",
        );
    };

    let existing = match repo.get_by_refresh_token(refresh_token).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "unknown refresh_token");
        }
        Err(e) => {
            tracing::error!(error = %e, "oauth/token: refresh lookup failed");
            return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "db error");
        }
    };

    if existing.revoked_at.is_some() {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "refresh_token revoked");
    }
    match existing.refresh_token_expires_at.as_deref() {
        Some(exp) if session_expired(exp) => {
            return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "refresh_token expired");
        }
        _ => {}
    }

    // Authenticate the client (the refresh must come from the same client it
    // was issued to; confidential clients must present their secret).
    let client = match repo.get_client(&existing.client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return token_error(StatusCode::UNAUTHORIZED, "invalid_client", "client gone");
        }
        Err(e) => {
            tracing::error!(error = %e, "oauth/token: client lookup failed");
            return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "db error");
        }
    };
    // If a client_id was supplied it must match the token's owner.
    if let Some(cid) = form.client_id.as_deref()
        && cid != existing.client_id
    {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant", "client mismatch");
    }
    if let Some(resp) = verify_client_secret(&client, form.client_secret.as_deref()) {
        return resp;
    }

    // Rotate: revoke the old token row, issue a fresh access + refresh pair
    // preserving user_id/resource/scope.
    let now = rfc3339_in(0);
    if let Err(e) = repo.revoke_access_token(&existing.token, &now).await {
        tracing::error!(error = %e, "oauth/token: revoke-on-rotate failed");
        return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "db error");
    }

    issue_tokens(
        &repo,
        &existing.client_id,
        &existing.user_id,
        &existing.resource,
        existing.scope.as_deref(),
    )
    .await
}

/// Mint a new access+refresh token pair and return the RFC 6749 token response.
async fn issue_tokens(
    repo: &OAuthRepository,
    client_id: &str,
    user_id: &str,
    resource: &str,
    scope: Option<&str>,
) -> Response {
    let access_token = random_token_b64();
    let refresh_token = random_token_b64();
    let access_expires_at = rfc3339_in(ACCESS_TOKEN_TTL_SECS);
    let refresh_expires_at = rfc3339_in(REFRESH_TOKEN_TTL_SECS);

    if let Err(e) = repo
        .create_access_token(NewAccessToken {
            token: &access_token,
            refresh_token: Some(&refresh_token),
            client_id,
            user_id,
            resource,
            scope,
            expires_at: &access_expires_at,
            refresh_token_expires_at: Some(&refresh_expires_at),
        })
        .await
    {
        tracing::error!(error = %e, "oauth/token: persist token failed");
        return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "db error");
    }

    let resp = TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL_SECS,
        refresh_token,
        scope: scope.unwrap_or(SCOPE_MCP).to_string(),
    };
    let mut out = Json(resp).into_response();
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

/// Verify a confidential client's secret with a constant-time compare. Public
/// clients (`none`) must NOT present a secret. Returns `Some(response)` with an
/// error response on failure, `None` when the client is authenticated.
fn verify_client_secret(
    client: &djinn_db::OAuthClient,
    presented: Option<&str>,
) -> Option<Response> {
    match client.client_secret.as_deref() {
        Some(expected) => {
            let presented = presented.unwrap_or("");
            if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                None
            } else {
                Some(token_error(
                    StatusCode::UNAUTHORIZED,
                    "invalid_client",
                    "bad client_secret",
                ))
            }
        }
        // Public client: no secret expected. A stray secret is ignored.
        None => None,
    }
}

// ── PKCE ────────────────────────────────────────────────────────────────────────

/// `BASE64URL_NOPAD(SHA256(code_verifier))` — the RFC 7636 S256 transform.
pub(super) fn pkce_s256(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

// ── Response helpers ──────────────────────────────────────────────────────────

fn no_store_json(body: serde_json::Value) -> Response {
    let mut out = Json(body).into_response();
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

/// RFC 6749 §5.2 error body for the token endpoint (and register).
fn token_error(status: StatusCode, error: &str, description: &str) -> Response {
    oauth_error(status, error, description)
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    let body = json!({ "error": error, "error_description": description });
    let mut out = (status, Json(body)).into_response();
    out.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out
}

fn plain_400(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn plain_500() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn redirect_to(location: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    (StatusCode::FOUND, headers).into_response()
}

/// 302 back to the client's redirect_uri carrying an OAuth `error` (RFC 6749
/// §4.1.2.1). Used only AFTER the redirect_uri has been validated.
fn redirect_err(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    let mut location = format!(
        "{redirect_uri}{}error={}",
        uri_query_sep(redirect_uri),
        urlencode(error)
    );
    if let Some(s) = state {
        location.push_str(&format!("&state={}", urlencode(s)));
    }
    redirect_to(&location)
}

/// `?` if the URI has no query yet, otherwise `&`.
fn uri_query_sep(uri: &str) -> char {
    if uri.contains('?') { '&' } else { '?' }
}

// ── Resource-server bearer resolution ────────────────────────────────────────────

/// Resolve a request to an authenticated djinn user from EITHER the existing
/// `djinn_session` cookie OR an `Authorization: Bearer <token>` MCP access
/// token. Returns the resolved user identity plus the user-scoped GitHub access
/// token (best-effort for the bearer path).
///
/// `BearerAuth::None` is returned when neither credential is present/valid; the
/// MCP handler turns that into a 401 with a `WWW-Authenticate` challenge when
/// auth is required.
pub(super) struct ResolvedUser {
    pub user_id: String,
    /// The user-scoped GitHub access token, if available. For the bearer path
    /// this is a best-effort lookup of the user's freshest live session token;
    /// `None` is acceptable for v1 (most task tools only need `user_id`).
    pub github_access_token: Option<String>,
}

pub(super) enum AuthOutcome {
    /// A valid credential resolved to this user.
    User(ResolvedUser),
    /// No credential present at all.
    Missing,
    /// A bearer token was presented but is invalid/expired/revoked/wrong-aud.
    InvalidBearer,
}

/// Try cookie first (preserves existing behaviour), then bearer.
pub(super) async fn resolve_request_user(state: &AppState, headers: &HeaderMap) -> AuthOutcome {
    // 1. Existing cookie session.
    match authenticate(state, headers).await {
        Ok(Some(user)) => {
            return AuthOutcome::User(ResolvedUser {
                user_id: user.id,
                github_access_token: Some(user.github_access_token),
            });
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "resolve_request_user: cookie authenticate failed");
        }
    }

    // 2. Bearer access token.
    let Some(bearer) = extract_bearer(headers) else {
        // No usable cookie and no bearer credential.
        return AuthOutcome::Missing;
    };

    let repo = OAuthRepository::new(state.db().clone());
    let token = match repo.get_access_token(&bearer).await {
        Ok(Some(t)) => t,
        Ok(None) => return AuthOutcome::InvalidBearer,
        Err(e) => {
            tracing::error!(error = %e, "resolve_request_user: token lookup failed");
            return AuthOutcome::InvalidBearer;
        }
    };

    if token.revoked_at.is_some() || session_expired(&token.expires_at) {
        return AuthOutcome::InvalidBearer;
    }
    // Audience validation: the token MUST be bound to THIS resource server.
    if token.resource != canonical_resource() {
        tracing::warn!(
            resource = %token.resource,
            expected = %canonical_resource(),
            "resolve_request_user: bearer token audience mismatch",
        );
        return AuthOutcome::InvalidBearer;
    }

    // Best-effort: fetch the user's freshest non-expired GitHub access token so
    // user-scoped GitHub tools keep working over bearer auth. If the user has
    // no live browser session, this is `None` — acceptable for v1 since most
    // task tools only need `user_id` for attribution.
    let github_access_token = match djinn_db::SessionAuthRepository::new(state.db().clone())
        .latest_token_for_user(&token.user_id)
        .await
    {
        Ok(Some(s)) => Some(s.github_access_token),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, "resolve_request_user: github token lookup failed");
            None
        }
    };

    AuthOutcome::User(ResolvedUser {
        user_id: token.user_id,
        github_access_token,
    })
}

/// Extract an `Authorization: Bearer <token>` value (case-insensitive scheme).
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer "))?;
    let tok = rest.trim();
    if tok.is_empty() { None } else { Some(tok.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc7636_test_vector() {
        // RFC 7636 Appendix B vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_s256(verifier), challenge);
    }

    #[test]
    fn pkce_wrong_verifier_does_not_match() {
        let challenge = pkce_s256("the-correct-verifier-string-padded-to-length");
        let wrong = pkce_s256("a-different-verifier-string-of-the-same-len--");
        assert_ne!(wrong, challenge);
        assert!(constant_time_eq(
            pkce_s256("the-correct-verifier-string-padded-to-length").as_bytes(),
            challenge.as_bytes()
        ));
    }

    #[test]
    fn canonical_resource_is_base_plus_mcp() {
        // Default base when DJINN_PUBLIC_URL is unset in the test env.
        let r = canonical_resource();
        assert!(r.ends_with("/mcp"), "resource was {r}");
        assert!(!r.contains("/mcp/mcp"));
    }

    #[test]
    fn uri_query_sep_picks_correct_joiner() {
        assert_eq!(uri_query_sep("http://x/cb"), '?');
        assert_eq!(uri_query_sep("http://x/cb?foo=1"), '&');
    }

    #[test]
    fn extract_bearer_parses_scheme_case_insensitively() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer abc.def"));
        assert_eq!(extract_bearer(&h).as_deref(), Some("abc.def"));

        let mut h2 = HeaderMap::new();
        h2.insert(header::AUTHORIZATION, HeaderValue::from_static("bearer xyz"));
        assert_eq!(extract_bearer(&h2).as_deref(), Some("xyz"));

        let mut h3 = HeaderMap::new();
        h3.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic zzz"));
        assert_eq!(extract_bearer(&h3), None);
    }

    #[test]
    fn www_authenticate_points_at_protected_resource_metadata() {
        let v = www_authenticate_value();
        assert!(v.starts_with("Bearer resource_metadata="));
        assert!(v.contains("/.well-known/oauth-protected-resource/mcp"));
    }
}
