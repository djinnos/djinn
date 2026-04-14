use axum::body::Body;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::memory_mount::{
    MemoryMountRuntimeStatus, MemoryMountViewFallbackReason, MountedMemoryFilesystem,
};
use crate::server::{self, AppState};
use crate::test_helpers;
use tokio_util::sync::CancellationToken;

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
    assert_eq!(json["memory_mount"]["lifecycle"], "disabled");
    assert_eq!(json["memory_mount"]["configured"], false);
    assert_eq!(json["memory_mount"]["pending_writes"], 0);
    assert!(json["memory_mount"]["mount_path"].is_null());
    assert!(json["memory_mount"]["project_id"].is_null());
    assert!(json["memory_mount"]["detail"].is_null());
    assert!(json["memory_mount"]["last_error"].is_null());
    assert!(json["memory_mount"]["view"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_memory_mount_runtime_status_details() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    state
        .set_memory_mount_for_tests(Some(MountedMemoryFilesystem::with_status(
            MemoryMountRuntimeStatus {
                lifecycle: crate::server::MemoryMountLifecycleState::Degraded,
                configured: true,
                mount_path: Some(std::path::PathBuf::from("/mnt/djinn-memory")),
                project_id: Some("project-123".to_string()),
                detail: Some(
                    "failed to flush pending write for research/note.md: boom".to_string(),
                ),
                pending_writes: 0,
                last_error: Some(
                    "failed to flush pending write for research/note.md: boom".to_string(),
                ),
                view_selection: Some(crate::memory_fs::MemoryViewSelection::Canonical),
                view_fallback: Some(MemoryMountViewFallbackReason::NoRunningSessionForActiveTask),
            },
        )))
        .await;
    let app = server::router(state);

    let req = axum::http::Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["memory_mount"]["enabled"], true);
    assert_eq!(json["memory_mount"]["active"], false);
    assert_eq!(json["memory_mount"]["lifecycle"], "degraded");
    assert_eq!(json["memory_mount"]["configured"], true);
    assert_eq!(json["memory_mount"]["mount_path"], "/mnt/djinn-memory");
    assert_eq!(json["memory_mount"]["project_id"], "project-123");
    assert_eq!(
        json["memory_mount"]["detail"],
        "failed to flush pending write for research/note.md: boom"
    );
    assert_eq!(json["memory_mount"]["pending_writes"], 0);
    assert_eq!(
        json["memory_mount"]["last_error"],
        "failed to flush pending write for research/note.md: boom"
    );
    assert_eq!(json["memory_mount"]["view"]["kind"], "canonical");
    assert_eq!(json["memory_mount"]["view"]["branch"], "main");
    assert!(json["memory_mount"]["view"]["task_short_id"].is_null());
    assert!(json["memory_mount"]["view"]["worktree_root"].is_null());
    assert_eq!(
        json["memory_mount"]["view"]["fallback_reason"],
        "no_running_session_for_active_task"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_task_scoped_memory_mount_view_details() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    state
        .set_memory_mount_for_tests(Some(MountedMemoryFilesystem::with_status(
            MemoryMountRuntimeStatus {
                lifecycle: crate::server::MemoryMountLifecycleState::Mounted,
                configured: true,
                mount_path: Some(std::path::PathBuf::from("/mnt/djinn-memory")),
                project_id: Some("project-123".to_string()),
                detail: None,
                pending_writes: 1,
                last_error: None,
                view_selection: Some(crate::memory_fs::MemoryViewSelection::Task {
                    task_short_id: Some("u5qe".to_string()),
                    worktree_root: Some(std::path::PathBuf::from(
                        "/worktrees/project/.djinn/worktrees/u5qe",
                    )),
                }),
                view_fallback: None,
            },
        )))
        .await;
    let app = server::router(state);

    let req = axum::http::Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["memory_mount"]["view"]["kind"], "task_scoped");
    assert_eq!(json["memory_mount"]["view"]["branch"], "task/u5qe");
    assert_eq!(json["memory_mount"]["view"]["task_short_id"], "u5qe");
    assert_eq!(
        json["memory_mount"]["view"]["worktree_root"],
        "/worktrees/project/.djinn/worktrees/u5qe"
    );
    assert!(json["memory_mount"]["view"]["fallback_reason"].is_null());
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
