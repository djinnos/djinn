use axum::body::Body;
use axum::http::{Request, StatusCode, header::CONTENT_TYPE};
use djinn_db::{CreateUserAuthSession, SessionAuthRepository, UserRepository};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::server::{self, AppState};
use crate::test_helpers;

const WEDGE_SCOPE: &str = "u-wedge";
const WEDGE_MODEL: &str = "claude-test";

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct WedgedDispatchState {
    app: axum::Router,
    db: djinn_db::Database,
    scope: &'static str,
    model: &'static str,
}

async fn setup_wedged_dispatch_state() -> WedgedDispatchState {
    let db = test_helpers::create_test_db();
    let state = AppState::new(db.clone(), CancellationToken::new());
    state.initialize_agent_handles_for_tests().await;

    // Use the breaker wedge because it is the least flaky end-to-end signal:
    // three synthetic provider failures open the per-user/model breaker
    // synchronously, and both `/metrics` and `/debug/dispatch-state` read that
    // shared tracker without needing a real worker pod or log scraping.
    for _ in 0..3 {
        state
            .health_tracker()
            .record_failure(Some(WEDGE_SCOPE), WEDGE_MODEL);
    }

    let app = server::router(state, false);
    WedgedDispatchState {
        app,
        db,
        scope: WEDGE_SCOPE,
        model: WEDGE_MODEL,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_reports_wedged_dispatch_via_metrics_alone() {
    let _guard = TEST_LOCK.lock().await;
    let wedged = setup_wedged_dispatch_state().await;

    let response = wedged
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(djinn_telemetry::PROMETHEUS_TEXT_CONTENT_TYPE)
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).expect("metrics response should be text");

    assert!(
        breaker_metric_line(text, wedged.scope, wedged.model)
            .is_some_and(|line| line.ends_with(" 1")),
        "expected open breaker metric for scope/model in /metrics body, without reading logs:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_dispatch_state_returns_wedge_to_admin_and_403_to_non_admin() {
    let _guard = TEST_LOCK.lock().await;
    let wedged = setup_wedged_dispatch_state().await;
    let admin_cookie = seed_session(&wedged.db, 101, "admin-wedge", true).await;
    let user_cookie = seed_session(&wedged.db, 102, "user-wedge", false).await;

    let admin_response = request_debug_dispatch_state(&wedged.app, Some(&admin_cookie)).await;
    assert_eq!(admin_response.status(), StatusCode::OK);
    assert_eq!(
        admin_response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json; charset=utf-8")
    );

    let json = response_json(admin_response).await;
    let breakers = json["breaker"]
        .as_array()
        .expect("breaker must be an array");
    let wedge_breaker = breakers
        .iter()
        .find(|entry| {
            entry["scope"] == wedged.scope
                && entry["model"] == wedged.model
                && entry["state"] == "open"
        })
        .expect("debug dispatch state must expose the open breaker wedge");
    assert_eq!(wedge_breaker["consecutive_failures"], 3);
    assert!(
        wedge_breaker["until"]
            .as_str()
            .is_some_and(|until| !until.is_empty()),
        "open breaker should include a future-ish deadline"
    );
    assert_eq!(json["totals"]["open_breakers"], 1);

    let anonymous_response = request_debug_dispatch_state(&wedged.app, None).await;
    assert_eq!(anonymous_response.status(), StatusCode::UNAUTHORIZED);

    let user_response = request_debug_dispatch_state(&wedged.app, Some(&user_cookie)).await;
    assert_eq!(user_response.status(), StatusCode::FORBIDDEN);
}

fn breaker_metric_line<'a>(text: &'a str, scope: &str, model: &str) -> Option<&'a str> {
    text.lines().find(|line| {
        line.starts_with("djinn_breaker_state")
            && line.contains(&format!("scope=\"{scope}\""))
            && line.contains(&format!("model=\"{model}\""))
    })
}

async fn request_debug_dispatch_state(
    app: &axum::Router,
    cookie: Option<&str>,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().uri("/debug/dispatch-state");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", format!("djinn_session={cookie}"));
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn response_json(response: axum::http::Response<Body>) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).expect("debug response should be JSON")
}

async fn seed_session(db: &djinn_db::Database, github_id: i64, login: &str, admin: bool) -> String {
    let users = UserRepository::new(db.clone());
    let user = users
        .upsert_from_github(github_id, login, None, None)
        .await
        .unwrap();
    users.set_admin_status(&user.id, admin).await.unwrap();

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
    token
}
