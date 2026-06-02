//! Integration tests for the MCP OAuth 2.1 AS+RS endpoints.
//!
//! Metadata-shape tests run without a database. The end-to-end flow tests
//! (register → authorize → token → bearer on /mcp) need the test Postgres on
//! :5433 (`make test-db-postgres-template`); they are written against the same
//! in-memory test harness every other server test uses and will be exercised
//! by `cargo nextest run --workspace` once a DB is reachable.

use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, LOCATION};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::server::{self, AppState};
use crate::test_helpers;
use djinn_db::{CreateUserAuthSession, Database, SessionAuthRepository, UserRepository};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn app_for(db: Database) -> axum::Router {
    let cancel = CancellationToken::new();
    let state = AppState::new(db, cancel);
    server::router(state, false)
}

async fn get(app: &axum::Router, uri: &str) -> axum::http::Response<Body> {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("response body should be JSON")
}

/// Seed a user + a live `djinn_session` row and return (user_id, cookie_token).
async fn seed_session(db: &Database, login: &str) -> (String, String) {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(424242, login, None, None)
        .await
        .unwrap();
    let token = format!("sess-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: login,
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    (user.id, token)
}

/// Register a public PKCE client and return its client_id.
async fn register_client(app: &axum::Router, redirect_uri: &str) -> String {
    let payload = serde_json::json!({
        "redirect_uris": [redirect_uri],
        "client_name": "Test MCP Client",
    });
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/register")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "register should 201");
    let json = body_json(resp).await;
    assert!(
        json["client_secret"].is_null(),
        "public client has no secret"
    );
    json["client_id"].as_str().unwrap().to_string()
}

// ── Metadata (no DB) ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protected_resource_metadata_shape() {
    let app = test_helpers::create_test_app();
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let resp = get(&app, path).await;
        assert_eq!(resp.status(), 200, "path {path}");
        let json = body_json(resp).await;
        assert!(json["resource"].as_str().unwrap().ends_with("/mcp"));
        assert_eq!(json["authorization_servers"].as_array().unwrap().len(), 1);
        assert_eq!(json["bearer_methods_supported"][0], "header");
        assert_eq!(json["scopes_supported"][0], "mcp");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorization_server_metadata_shape() {
    let app = test_helpers::create_test_app();
    let resp = get(&app, "/.well-known/oauth-authorization-server").await;
    assert_eq!(resp.status(), 200);
    let json = body_json(resp).await;
    let issuer = json["issuer"].as_str().unwrap();
    assert_eq!(
        json["authorization_endpoint"],
        Value::from(format!("{issuer}/oauth/authorize"))
    );
    assert_eq!(
        json["token_endpoint"],
        Value::from(format!("{issuer}/oauth/token"))
    );
    assert_eq!(
        json["registration_endpoint"],
        Value::from(format!("{issuer}/oauth/register"))
    );
    assert_eq!(json["response_types_supported"][0], "code");
    assert_eq!(json["code_challenge_methods_supported"][0], "S256");
    let grants = json["grant_types_supported"].as_array().unwrap();
    assert!(grants.contains(&Value::from("authorization_code")));
    assert!(grants.contains(&Value::from("refresh_token")));
}

// ── Authorize / token (need DB) ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorize_rejects_unknown_client() {
    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();
    let app = app_for(db);
    let resp = get(
        &app,
        "/oauth/authorize?response_type=code&client_id=nope&redirect_uri=http://x/cb&code_challenge=abc&code_challenge_method=S256&state=s",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorize_rejects_unregistered_redirect_uri() {
    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();
    let app = app_for(db);
    let client_id = register_client(&app, "http://127.0.0.1:9999/callback").await;

    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri=http://evil/cb&code_challenge=abc&code_challenge_method=S256&state=s"
    );
    let resp = get(&app, &uri).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "redirect_uri not in registered set must 400, not redirect"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorize_without_session_bounces_to_github_login() {
    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();
    let app = app_for(db);
    let redirect = "http://127.0.0.1:9999/callback";
    let client_id = register_client(&app, redirect).await;

    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={redirect}&code_challenge=abc&code_challenge_method=S256&state=s"
    );
    let resp = get(&app, &uri).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    let loc = resp.headers().get(LOCATION).unwrap().to_str().unwrap();
    assert!(
        loc.starts_with("/auth/github/start?redirect="),
        "expected bounce into GitHub login, got {loc}"
    );
    // The post-login redirect must be a site-local /oauth/authorize path.
    assert!(loc.contains("oauth%2Fauthorize"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorize_rejects_plain_pkce() {
    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();
    let (_uid, cookie) = seed_session(&db, "pkce-plain").await;
    let app = app_for(db);
    let redirect = "http://127.0.0.1:9999/callback";
    let client_id = register_client(&app, redirect).await;

    let uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={redirect}&code_challenge=abc&code_challenge_method=plain&state=s"
    );
    let req = Request::builder()
        .uri(&uri)
        .header("cookie", format!("djinn_session={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    // Error is reported back to the (validated) redirect_uri.
    let loc = resp.headers().get(LOCATION).unwrap().to_str().unwrap();
    assert!(loc.starts_with(redirect), "got {loc}");
    assert!(loc.contains("error=invalid_request"), "got {loc}");
}

/// Full happy path: register → authorize (with seeded session) → token →
/// use the bearer token on /mcp tools/list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_happy_path_register_authorize_token_bearer() {
    use base64::Engine;

    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();
    let (user_id, cookie) = seed_session(&db, "happy-path").await;
    let db_for_check = db.clone();
    let app = app_for(db);
    let redirect = "http://127.0.0.1:9999/callback";
    let client_id = register_client(&app, redirect).await;

    // PKCE pair.
    let verifier = "this-is-a-sufficiently-long-code-verifier-string-0123456789";
    let challenge = {
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d)
    };

    // Authorize with the session cookie → expect 302 to redirect_uri?code=…
    let auth_uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={redirect}&code_challenge={challenge}&code_challenge_method=S256&state=xyz&scope=mcp"
    );
    let req = Request::builder()
        .uri(&auth_uri)
        .header("cookie", format!("djinn_session={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FOUND,
        "authorize should redirect"
    );
    let loc = resp.headers().get(LOCATION).unwrap().to_str().unwrap();
    assert!(loc.starts_with(redirect), "got {loc}");
    assert!(loc.contains("state=xyz"), "state must be echoed: {loc}");
    let code = extract_query_param(loc, "code").expect("code in redirect");

    // Token exchange (authorization_code).
    let form = format!(
        "grant_type=authorization_code&code={code}&code_verifier={verifier}&client_id={client_id}&redirect_uri={}",
        urlencoding_min(redirect)
    );
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "token exchange should succeed");
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "no-store"
    );
    let json = body_json(resp).await;
    assert_eq!(json["token_type"], "Bearer");
    assert_eq!(json["scope"], "mcp");
    let access_token = json["access_token"].as_str().unwrap().to_string();
    let refresh_token = json["refresh_token"].as_str().unwrap().to_string();
    assert!(!access_token.is_empty());
    assert!(!refresh_token.is_empty());

    // Reusing the same code must now fail (single-use).
    let form = format!(
        "grant_type=authorization_code&code={code}&code_verifier={verifier}&client_id={client_id}&redirect_uri={}",
        urlencoding_min(redirect)
    );
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "code replay must fail"
    );

    // Use the bearer token on /mcp (tools/list). Should be accepted.
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/list",
        "params": {}
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header("authorization", format!("Bearer {access_token}"))
        .body(Body::from(payload.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "bearer-authed tools/list should 200");
    let json = body_json(resp).await;
    assert!(
        json["result"]["tools"].is_array(),
        "tools/list returns tools"
    );

    // Refresh-token rotation.
    let form =
        format!("grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}");
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "refresh should succeed");
    let json2 = body_json(resp).await;
    let new_access = json2["access_token"].as_str().unwrap();
    assert_ne!(
        new_access, access_token,
        "rotation issues a new access token"
    );

    // The OLD access token is now revoked → bearer rejected on /mcp. But our
    // /mcp gate only *requires* auth when a GitHub App is configured; with no
    // App config the old token simply resolves to no-attribution. Verify the
    // revocation at the token-store level via a fresh refresh attempt with the
    // old refresh token (should fail — it was rotated/revoked).
    let form =
        format!("grant_type=refresh_token&refresh_token={refresh_token}&client_id={client_id}");
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "old refresh token must not work after rotation"
    );

    // Sanity: the issued token is attributed to the seeded user.
    let tok = djinn_db::OAuthRepository::new(db_for_check)
        .get_access_token(new_access)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tok.user_id, user_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_rejects_wrong_pkce_verifier() {
    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();
    let (_uid, cookie) = seed_session(&db, "wrong-pkce").await;
    let app = app_for(db);
    let redirect = "http://127.0.0.1:9999/callback";
    let client_id = register_client(&app, redirect).await;

    let challenge = {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let d = Sha256::digest(b"the-real-verifier-value-padded-to-be-long-enough");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(d)
    };
    let auth_uri = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={redirect}&code_challenge={challenge}&code_challenge_method=S256&state=s"
    );
    let req = Request::builder()
        .uri(&auth_uri)
        .header("cookie", format!("djinn_session={cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let loc = resp
        .headers()
        .get(LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let code = extract_query_param(&loc, "code").unwrap();

    // Exchange with the WRONG verifier.
    let form = format!(
        "grant_type=authorization_code&code={code}&code_verifier=totally-the-wrong-verifier&client_id={client_id}&redirect_uri={}",
        urlencoding_min(redirect)
    );
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/token")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "wrong PKCE must fail"
    );
    let json = body_json(resp).await;
    assert_eq!(json["error"], "invalid_grant");
}

// ── small helpers ────────────────────────────────────────────────────────────

fn extract_query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Minimal percent-encoder matching `auth::urlencode` for the chars we use.
fn urlencoding_min(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b;
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(c as char)
            }
            _ => out.push_str(&format!("%{:02X}", c)),
        }
    }
    out
}
