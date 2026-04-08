use axum::body::Body;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::test_helpers;

/// Integration test: hit /health via tower::ServiceExt::oneshot().
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_returns_ok() {
    let app = test_helpers::create_test_app();

    let req = axum::http::Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_initialize_returns_ok() {
    let app = test_helpers::create_test_app();

    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "0.0.0"
            }
        }
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_rejects_empty_messages() {
    let app = test_helpers::create_test_app();

    let payload = serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "messages": []
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/chat/completions")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
    assert!(text.contains("messages must not be empty"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_rejects_unknown_provider() {
    let app = test_helpers::create_test_app();

    let payload = serde_json::json!({
        "model": "doesnotexist/model",
        "messages": [
            {"role": "user", "content": "hello"}
        ]
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/chat/completions")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
    assert!(text.contains("unknown provider"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_rejects_missing_provider_credential() {
    let app = test_helpers::create_test_app();

    let payload = serde_json::json!({
        "model": "openai/gpt-4o-mini",
        "messages": [
            {"role": "user", "content": "hello"}
        ]
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/chat/completions")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 400);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
    assert!(text.contains("provider credential resolution failed"));
}
