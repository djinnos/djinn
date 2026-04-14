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
    assert_eq!(json["database"]["backend_label"], "sqlite");
    assert_eq!(json["memory_mount"]["enabled"], false);
    assert_eq!(json["memory_mount"]["active"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_info_reports_selected_backend() {
    let app = test_helpers::create_test_app();

    let req = axum::http::Request::builder()
        .uri("/db-info")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["backend"], "sqlite");
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
