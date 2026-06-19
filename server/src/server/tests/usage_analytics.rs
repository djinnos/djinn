use axum::body::Body;
use axum::http::{Request, StatusCode};
use djinn_db::{CreateUserAuthSession, SessionAuthRepository, UserRepository};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::test_helpers;

fn rand_github_id() -> i64 {
    let bytes = *uuid::Uuid::now_v7().as_bytes();
    i64::from_be_bytes(bytes[8..16].try_into().unwrap()).unsigned_abs() as i64
}

async fn seed_session(db: &djinn_db::Database, login: &str, is_admin: bool) -> String {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(rand_github_id(), login, None, None)
        .await
        .unwrap();
    UserRepository::new(db.clone())
        .set_admin_status(&user.id, is_admin)
        .await
        .unwrap();
    let token = format!("usage-sess-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: login,
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_usage_test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    token
}

async fn get_usage(app: &axum::Router, cookie: Option<&str>) -> axum::http::Response<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/api/admin/usage?from=2025-01-01&to=2025-01-02");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", format!("djinn_session={cookie}"));
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_endpoint_rejects_non_admin_even_when_route_exists() {
    let db = test_helpers::create_test_db();
    let cookie = seed_session(&db, "usage-non-admin", false).await;
    let app = test_helpers::create_test_app_with_db(db);

    let response = get_usage(&app, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_endpoint_rejects_unauthenticated_requests() {
    let app = test_helpers::create_test_app();

    let response = get_usage(&app, None).await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_endpoint_admin_response_has_expected_top_level_shape() {
    let db = test_helpers::create_test_db();
    let cookie = seed_session(&db, "usage-admin", true).await;
    let app = test_helpers::create_test_app_with_db(db);

    let response = get_usage(&app, Some(&cookie)).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();
    for key in [
        "totals",
        "previous_totals",
        "series",
        "breakdown",
        "model_effectiveness",
        "project_model_matrix",
    ] {
        assert!(
            json.get(key).is_some(),
            "missing top-level field {key}: {json}"
        );
    }
    assert!(json["totals"].is_object());
    assert!(json["previous_totals"].is_object());
    assert!(json["series"].is_array());
    assert!(json["breakdown"].is_array());
    assert!(json["model_effectiveness"].is_array());
    assert!(json["project_model_matrix"].is_array());
}
