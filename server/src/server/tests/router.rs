use axum::body::Body;
use std::time::{Duration, SystemTime};

use axum::http::header::{ACCESS_CONTROL_EXPOSE_HEADERS, CONTENT_TYPE, ORIGIN};
use djinn_core::clock::{Clock, SystemClock};
use http_body_util::BodyExt;
use tower::ServiceExt;

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
    let database_target = json["database"]["target"].as_str().unwrap();
    assert!(database_target.starts_with("postgres://<redacted>@"));
    assert!(!database_target.contains("postgres:postgres"));
    assert!(json["provider_catalog"].is_object());
    assert_eq!(json["provider_catalog"]["source_tier"], "embedded");
    assert!(json["provider_catalog"]["fetched_at"].is_null());
    assert!(json["provider_catalog"]["age_seconds"].is_null());
    assert_eq!(json["provider_catalog"]["refresh_interval_seconds"], 3600);
    assert_eq!(json["provider_catalog"]["last_refresh_status"], "never");
    assert!(json["provider_catalog"]["last_refresh_error"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn galaxy_route_exposes_validator_headers_to_credentialed_origins() {
    let app = test_helpers::create_test_app();
    let req = axum::http::Request::builder()
        .uri("/api/projects/project-id/code-graph/galaxy")
        .header(ORIGIN, "https://ui.example.test")
        .header(axum::http::header::COOKIE, "session=credential")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    let exposed = response.headers()[ACCESS_CONTROL_EXPOSE_HEADERS]
        .to_str()
        .unwrap()
        .split(',')
        .map(str::trim)
        .collect::<std::collections::HashSet<_>>();

    assert_eq!(
        exposed,
        std::collections::HashSet::from([
            "etag",
            super::super::galaxy::HEADER_PROJECT_ID,
            super::super::galaxy::HEADER_GENERATION_ID,
            super::super::galaxy::HEADER_COMMIT_SHA,
            super::super::galaxy::HEADER_ARTIFACT_VERSION,
            super::super::galaxy::HEADER_SEMANTIC_HASH,
        ])
    );
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

/// After a successful refresh, `/health` should expose a non-null RFC3339
/// `fetched_at` and a non-null `age_seconds`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_provider_catalog_serializes_fetched_at_after_success() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let catalog = state.catalog().clone();

    // Use a fixed wall-clock instant and a recent monotonic instant so the
    // response is deterministic and the source tier is live.
    let fetched_at_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let fetched_at_mono = SystemClock::new().now_instant();
    catalog.set_last_success_times_for_tests(
        Some(fetched_at_mono),
        Some(fetched_at_wall),
        djinn_provider::catalog::RefreshStatus::Success,
        None,
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
    assert_eq!(json["provider_catalog"]["source_tier"], "live");
    assert_eq!(json["provider_catalog"]["last_refresh_status"], "success");
    assert!(json["provider_catalog"]["last_refresh_error"].is_null());

    let fetched_at = json["provider_catalog"]["fetched_at"]
        .as_str()
        .expect("fetched_at should be a non-null RFC3339 string");
    let parsed =
        time::OffsetDateTime::parse(fetched_at, &time::format_description::well_known::Rfc3339)
            .expect("fetched_at must be a valid RFC3339 timestamp");
    assert_eq!(parsed.unix_timestamp(), 1_700_000_000);

    let age = json["provider_catalog"]["age_seconds"]
        .as_u64()
        .expect("age_seconds should be non-null after a successful fetch");
    assert!(age < 5, "age should be recent; got {age}");
}

/// After a successful refresh followed by a failure, `/health` should retain
/// the prior `fetched_at` and `age_seconds` while reporting error status/text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_provider_catalog_retains_fetched_at_after_success_then_error() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let catalog = state.catalog().clone();

    let fetched_at_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let fetched_at_mono = SystemClock::new().now_instant();
    catalog.set_last_success_times_for_tests(
        Some(fetched_at_mono),
        Some(fetched_at_wall),
        djinn_provider::catalog::RefreshStatus::Success,
        None,
    );

    // Now transition to an error state without clearing the success timestamps.
    catalog.set_last_success_times_for_tests(
        Some(fetched_at_mono),
        Some(fetched_at_wall),
        djinn_provider::catalog::RefreshStatus::Error,
        Some("models.dev returned HTTP 503".to_string()),
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
    // Because the monotonic timestamp is recent, the source tier is still live.
    assert_eq!(json["provider_catalog"]["source_tier"], "live");
    assert_eq!(json["provider_catalog"]["last_refresh_status"], "error");
    assert_eq!(
        json["provider_catalog"]["last_refresh_error"],
        "models.dev returned HTTP 503"
    );

    let fetched_at = json["provider_catalog"]["fetched_at"]
        .as_str()
        .expect("fetched_at should be retained as a non-null RFC3339 string");
    let parsed =
        time::OffsetDateTime::parse(fetched_at, &time::format_description::well_known::Rfc3339)
            .expect("retained fetched_at must still be valid RFC3339");
    assert_eq!(parsed.unix_timestamp(), 1_700_000_000);

    let age = json["provider_catalog"]["age_seconds"]
        .as_u64()
        .expect("age_seconds should be retained after an error");
    assert!(
        age < 5,
        "age should still reflect the recent success; got {age}"
    );
}

/// Ensure the health endpoint stays resilient even if the catalog wall-clock
/// timestamp is out of RFC3339 range: the rest of the response is still served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_provider_catalog_fetched_at_degrades_safely_on_invalid_wall_time() {
    let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
    let catalog = state.catalog().clone();

    // An out-of-range wall time should not be representable as RFC3339, so
    // `fetched_at` serializes as null while the monotonic age remains available.
    // `SystemTime::UNIX_EPOCH + u64::MAX` panics at runtime, so use a slightly
    // smaller value that still overflows `OffsetDateTime::from_unix_timestamp`.
    let fetched_at_wall = SystemTime::UNIX_EPOCH + Duration::from_secs(i64::MAX as u64);
    let fetched_at_mono = SystemClock::new().now_instant();
    catalog.set_last_success_times_for_tests(
        Some(fetched_at_mono),
        Some(fetched_at_wall),
        djinn_provider::catalog::RefreshStatus::Success,
        None,
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
    assert_eq!(json["provider_catalog"]["source_tier"], "live");
    assert_eq!(json["provider_catalog"]["last_refresh_status"], "success");
    assert!(json["provider_catalog"]["fetched_at"].is_null());
    assert!(
        json["provider_catalog"]["age_seconds"].is_number(),
        "age_seconds should still be present when formatting fails"
    );
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
        "djinn_process_rss_bytes",
        "djinn_process_anon_rss_bytes",
        "djinn_jemalloc_allocated_bytes",
        "djinn_jemalloc_resident_bytes",
        "djinn_jemalloc_retained_bytes",
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
    for metric in [
        "djinn_process_rss_bytes",
        "djinn_process_anon_rss_bytes",
        "djinn_jemalloc_allocated_bytes",
        "djinn_jemalloc_resident_bytes",
        "djinn_jemalloc_retained_bytes",
    ] {
        let line = text
            .lines()
            .find(|line| line.starts_with(metric) && !line.starts_with("#"))
            .unwrap_or_else(|| panic!("missing server-memory sample for {metric} in:\n{text}"));
        assert!(
            !line.contains('{'),
            "server-memory byte gauges must not have identity labels: {line}"
        );
        for forbidden in ["project", "task", "commit", "generation", "path", "pid"] {
            assert!(
                !line.contains(forbidden),
                "server-memory byte gauge must not contain {forbidden}: {line}"
            );
        }
    }
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
