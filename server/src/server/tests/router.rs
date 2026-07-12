use axum::body::Body;
use std::time::Duration;

use axum::http::header::{ACCEPT, CONTENT_TYPE};
use djinn_core::clock::{Clock, SystemClock};
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
    assert_eq!(json["database"]["backend_label"], "postgres");
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
    assert!(json["provider_catalog"].is_object());
    assert_eq!(json["provider_catalog"]["source_tier"], "embedded");
    assert!(json["provider_catalog"]["fetched_at"].is_null());
    assert!(json["provider_catalog"]["age_seconds"].is_null());
    assert_eq!(json["provider_catalog"]["refresh_interval_seconds"], 3600);
    assert_eq!(json["provider_catalog"]["last_refresh_status"], "never");
    assert!(json["provider_catalog"]["last_refresh_error"].is_null());
}

/// Source-tier policy test: verify the catalog-only window used by the health
/// endpoint (`2 * refresh_interval`) without relying on live network access.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_provider_catalog_source_tier_policy() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());

    // Default state: never refreshed, so the source tier is embedded.
    assert_eq!(
        state.catalog().source_tier(Duration::from_secs(3600 * 2)),
        djinn_provider::catalog::SourceTier::Embedded
    );
    assert!(state.catalog().last_successful_fetch_age().is_none());
    assert_eq!(
        state.catalog().last_refresh_status(),
        djinn_provider::catalog::RefreshStatus::Never
    );

    // Simulate a recent successful fetch by directly setting the internal
    // `fetched_at` monotonic timestamp. This keeps the test hermetic and
    // avoids network access.
    let catalog = state.catalog().clone();
    catalog.set_last_success_for_tests(
        Some(SystemClock::new().now_instant()),
        djinn_provider::catalog::RefreshStatus::Success,
        None,
    );
    assert_eq!(
        state.catalog().source_tier(Duration::from_secs(3600 * 2)),
        djinn_provider::catalog::SourceTier::Live
    );

    // Simulate a stale fetch by setting the timestamp far in the past.
    catalog.set_last_success_for_tests(
        Some(SystemClock::new().now_instant() - Duration::from_secs(3600 * 3)),
        djinn_provider::catalog::RefreshStatus::Success,
        None,
    );
    assert_eq!(
        state.catalog().source_tier(Duration::from_secs(3600 * 2)),
        djinn_provider::catalog::SourceTier::Stale
    );
}

/// Serialization test for the `last_refresh_status == Error` branch, ensuring
/// the error text is surfaced without requiring a live models.dev fetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_provider_catalog_serialize_error_status() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let catalog = state.catalog().clone();
    catalog.set_last_success_for_tests(
        None,
        djinn_provider::catalog::RefreshStatus::Error,
        Some("simulated refresh failure".to_string()),
    );

    let app = server::router(state, false);
    let req = axum::http::Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["provider_catalog"]["source_tier"], "embedded");
    assert_eq!(json["provider_catalog"]["last_refresh_status"], "error");
    assert_eq!(
        json["provider_catalog"]["last_refresh_error"],
        "simulated refresh failure"
    );
    assert!(json["provider_catalog"]["fetched_at"].is_null());
    assert!(json["provider_catalog"]["age_seconds"].is_null());
    assert_eq!(json["provider_catalog"]["refresh_interval_seconds"], 3600);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_returns_prometheus_text_without_auth() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    state
        .health_tracker()
        .record_stall(Some("metrics-user"), "metrics-model", true);
    let app = server::router(state, false);

    let req = axum::http::Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(djinn_telemetry::PROMETHEUS_TEXT_CONTENT_TYPE)
    );

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.starts_with("# HELP djinn_dispatch_attempts_total"),
        "metrics output should begin with dispatch HELP, got:\n{text}"
    );
    assert!(text.contains("# TYPE djinn_dispatch_attempts_total counter"));
    for metric in [
        "djinn_dispatch_attempts_total{outcome=\"ok\"}",
        "djinn_dispatch_attempts_total{outcome=\"cooldown\"}",
        "djinn_dispatch_attempts_total{outcome=\"cap\"}",
        "djinn_dispatch_attempts_total{outcome=\"breaker\"}",
        "djinn_dispatch_attempts_total{outcome=\"error\"}",
        "djinn_dispatch_cooldowns_active",
        "djinn_dispatch_last_success_timestamp",
        "djinn_slot_pool{",
        "djinn_inflight_ledger_size",
        "djinn_user_cap_utilization{",
        "djinn_breaker_state{",
        "djinn_breaker_trips_total",
        "djinn_zombie_reaps_total{kind=\"startup\"}",
        "djinn_zombie_reaps_total{kind=\"periodic\"}",
        "djinn_zombie_reaps_total{kind=\"stall\"}",
        "djinn_task_reopens_total",
        "djinn_tasks_parked_total",
        "djinn_pr_poller_tracked",
        "djinn_merge_failures_total",
    ] {
        assert!(text.contains(metric), "missing metric {metric} in:\n{text}");
    }
    assert_metric_line_contains_all(text, "djinn_slot_pool", &["state=\"free\"", "model=\"\""]);
    assert_metric_line_contains_all(text, "djinn_slot_pool", &["state=\"busy\"", "model=\"\""]);
    assert_metric_line_contains_all(
        text,
        "djinn_user_cap_utilization",
        &["user=\"\"", "model=\"\""],
    );
    assert_metric_line_contains_all(
        text,
        "djinn_breaker_state",
        &["scope=\"metrics-user\"", "model=\"metrics-model\""],
    );
    assert!(text.contains(" 1"));
}

fn assert_metric_line_contains_all(text: &str, metric: &str, labels: &[&str]) {
    assert!(
        text.lines()
            .filter(|line| line.starts_with(metric))
            .any(|line| labels.iter().all(|label| line.contains(label))),
        "missing metric line for {metric} with labels {labels:?} in:\n{text}"
    );
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
                        session_workspace_path: None,
                    }),
                },
                pending_writes: 0,
                last_error: Some(
                    "failed to flush pending write for research/note.md: boom".to_string(),
                ),
            },
        )))
        .await;
    let app = server::router(state, false);

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
    let app = server::router(state, false);

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
    assert_eq!(json["backend"], "postgres");
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
