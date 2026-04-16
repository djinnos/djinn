use axum::body::Body;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::memory_mount::{MemoryMountRuntimeStatus, MountedMemoryFilesystem};
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
    assert_eq!(json["database"]["backend_label"], "dolt");
    assert_eq!(json["memory_mount"]["enabled"], false);
    assert_eq!(json["memory_mount"]["active"], false);
    assert_eq!(json["memory_mount"]["lifecycle"], "disabled");
    assert_eq!(json["memory_mount"]["configured"], false);
    assert_eq!(json["memory_mount"]["view"]["kind"], "canonical");
    assert!(json["memory_mount"]["view"]["task_short_id"].is_null());
    assert!(json["memory_mount"]["view"]["worktree_root"].is_null());
    assert!(json["memory_mount"]["view"]["fallback"].is_null());
    assert_eq!(json["memory_mount"]["pending_writes"], 0);
    assert!(json["memory_mount"]["mount_path"].is_null());
    assert!(json["memory_mount"]["project_id"].is_null());
    assert!(json["memory_mount"]["detail"].is_null());
    assert!(json["memory_mount"]["last_error"].is_null());
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
                view: crate::server::MemoryMountViewHealth {
                    kind: crate::server::MemoryMountViewKind::Canonical,
                    task_short_id: None,
                    worktree_root: None,
                    fallback: Some(crate::server::MemoryMountViewFallback {
                        reason: crate::server::MemoryMountViewFallbackReason::NoActiveSession,
                        detail: Some(
                            "no running session is attached to the active task".to_string(),
                        ),
                        active_task_count: Some(1),
                        task_id: Some("task-123".to_string()),
                        task_short_id: Some("u5qe".to_string()),
                        task_project_id: Some("project-123".to_string()),
                        mount_project_id: Some("project-123".to_string()),
                        session_worktree_path: None,
                    }),
                },
                pending_writes: 0,
                last_error: Some(
                    "failed to flush pending write for research/note.md: boom".to_string(),
                ),
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
    assert_eq!(json["memory_mount"]["view"]["kind"], "canonical");
    assert!(json["memory_mount"]["view"]["worktree_root"].is_null());
    assert_eq!(
        json["memory_mount"]["view"]["fallback"]["reason"],
        "no_active_session"
    );
    assert_eq!(
        json["memory_mount"]["view"]["fallback"]["task_short_id"],
        "u5qe"
    );
    assert_eq!(json["memory_mount"]["pending_writes"], 0);
    assert_eq!(
        json["memory_mount"]["last_error"],
        "failed to flush pending write for research/note.md: boom"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_task_scoped_memory_mount_view() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    state
        .set_memory_mount_for_tests(Some(MountedMemoryFilesystem::with_status(
            MemoryMountRuntimeStatus {
                lifecycle: crate::server::MemoryMountLifecycleState::Mounted,
                configured: true,
                mount_path: Some(std::path::PathBuf::from("/mnt/djinn-memory")),
                project_id: Some("project-123".to_string()),
                detail: None,
                view: crate::server::MemoryMountViewHealth {
                    kind: crate::server::MemoryMountViewKind::TaskScoped,
                    task_short_id: Some("98vz".to_string()),
                    worktree_root: Some("/worktrees/task-98vz".to_string()),
                    fallback: None,
                },
                pending_writes: 2,
                last_error: None,
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
    assert_eq!(json["memory_mount"]["lifecycle"], "mounted");
    assert_eq!(json["memory_mount"]["view"]["kind"], "task_scoped");
    assert_eq!(json["memory_mount"]["view"]["task_short_id"], "98vz");
    assert_eq!(
        json["memory_mount"]["view"]["worktree_root"],
        "/worktrees/task-98vz"
    );
    assert!(json["memory_mount"]["view"]["fallback"].is_null());
    assert_eq!(json["memory_mount"]["pending_writes"], 2);
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
    assert_eq!(json["backend"], "dolt");
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
