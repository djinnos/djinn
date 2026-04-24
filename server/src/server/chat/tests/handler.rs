use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::test_helpers;
use djinn_provider::repos::CredentialRepository;

async fn post_chat(payload: serde_json::Value) -> (axum::http::StatusCode, String) {
    let app = test_helpers::create_test_app();
    post_chat_with_app(app, payload).await
}

async fn post_chat_with_app(
    app: axum::Router,
    payload: serde_json::Value,
) -> (axum::http::StatusCode, String) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/chat/completions")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
    (status, text)
}

/// Fresh UUIDv7 for each test — chat completions now require a
/// client-minted session id.
fn fresh_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_rejects_empty_messages() {
    let (status, body) = post_chat(serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "messages": [],
        "session_id": fresh_session_id(),
    }))
    .await;

    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body.contains("messages must not be empty"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_rejects_missing_session_id() {
    let (status, body) = post_chat(serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "messages": [{"role": "user", "content": "hello"}],
    }))
    .await;

    // Missing `session_id` fails serde deserialization → 422.
    assert!(
        status == axum::http::StatusCode::UNPROCESSABLE_ENTITY
            || status == axum::http::StatusCode::BAD_REQUEST,
        "expected 4xx; got {status}: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_rejects_non_uuid_session_id() {
    let (status, body) = post_chat(serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "messages": [{"role": "user", "content": "hello"}],
        "session_id": "not-a-uuid",
    }))
    .await;

    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body.contains("session_id must be a UUID"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_rejects_unknown_provider() {
    let (status, body) = post_chat(serde_json::json!({
        "model": "doesnotexist/model",
        "messages": [{"role": "user", "content": "hello"}],
        "session_id": fresh_session_id(),
    }))
    .await;

    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body.contains("unknown provider"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_rejects_missing_provider_credential() {
    let (status, body) = post_chat(serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "messages": [{"role": "user", "content": "hello"}],
        "session_id": fresh_session_id(),
    }))
    .await;

    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body.contains("provider credential resolution failed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completions_rejects_unsupported_role_after_request_parsing() {
    let db = test_helpers::create_test_db();
    CredentialRepository::new(db.clone(), crate::events::EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "sk-test")
        .await
        .expect("seed openai credential");
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = post_chat_with_app(
        app,
        serde_json::json!({
            "model": "openai/gpt-4o-mini",
            "messages": [{"role": "moderator", "content": "hello"}],
            "system": "be brief",
            "session_id": fresh_session_id(),
        }),
    )
    .await;

    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert!(body.contains("unsupported role: moderator"));
}
