use axum::Router;
use axum::extract::State;
use axum::http::{
    HeaderName,
    header::{self, CONTENT_TYPE},
};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use serde::Serialize;
use std::time::{Duration, SystemTime};
use time::format_description::well_known::Rfc3339;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

use crate::sse;

mod agents;
mod auth;
#[cfg(any(test, feature = "test-support"))]
pub mod chat;
#[cfg(not(any(test, feature = "test-support")))]
mod chat;
mod debug;
mod galaxy;
mod github_install;
mod mcp_handler;
mod oauth;
mod org_sync;
mod project_tools;
mod state;
mod static_ui;
pub mod usage_analytics;
mod users;
pub use auth::{AuthenticatedUser, authenticate, require_admin};
pub use org_sync::{SyncReport, start_org_member_sync, sync_once};
pub use state::{AppState, StartupReconnectabilityMeasurement};

/// Build the application router.
///
/// `serve_ui` controls whether the embedded Vite SPA is served as the
/// router's fallback. Leave it on (the default) for a single-image
/// deployment; turn it off (via `--ui-enabled=false` / `DJINN_UI_ENABLED=0`)
/// for headless API-only deployments where the UI lives elsewhere.
pub fn router(state: AppState, serve_ui: bool) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        // Cluster-internal trust boundary: /metrics is intentionally unauthenticated
        // for Prometheus scraping. Deployments should restrict it with a
        // NetworkPolicy that only allows the monitoring namespace to reach it.
        .route("/metrics", get(metrics))
        .route("/events", get(sse::events_handler))
        .route("/db-info", get(sse::db_info_handler))
        .route("/api/chat/completions", post(chat::completions_handler))
        .merge(chat::sessions::router())
        .route("/mcp", post(mcp_handler::mcp_handler))
        .merge(agents::router())
        .merge(auth::router())
        .merge(debug::router())
        .merge(oauth::router())
        .merge(github_install::router())
        .merge(galaxy::router())
        .merge(crate::mirror_fetcher::router())
        .merge(org_sync::router())
        .merge(project_tools::router())
        .merge(usage_analytics::router())
        .merge(users::router());
    if serve_ui {
        router = router.fallback(static_ui::serve_static);
    }
    router.layer(cors_layer()).with_state(state)
}

async fn metrics(State(state): State<AppState>) -> Response {
    if let Err(e) = djinn_telemetry::init() {
        tracing::warn!(error = %e, "failed to initialize Prometheus metrics recorder");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    refresh_metrics_live_state(&state).await;
    state.health_tracker().record_breaker_metrics();
    match djinn_telemetry::render() {
        Ok(body) => (
            [(CONTENT_TYPE, djinn_telemetry::PROMETHEUS_TEXT_CONTENT_TYPE)],
            body,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to render Prometheus metrics");
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn refresh_metrics_live_state(state: &AppState) {
    crate::server_memory::refresh();

    if let Some(coordinator) = state.coordinator().await
        && let Err(e) = coordinator.record_live_metrics().await
    {
        tracing::warn!(error = %e, "failed to refresh coordinator metrics snapshot");
    }

    if let Some(pool) = state.pool().await
        && let Err(e) = pool.get_status().await
    {
        tracing::warn!(error = %e, "failed to refresh slot-pool metrics snapshot");
    }
}

/// CORS layer that allows any origin but — crucially — **reflects** the
/// request origin instead of returning `*`, so browsers accept responses
/// to `credentials: 'include'` requests (session cookie auth).
fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers([
            header::ETAG,
            HeaderName::from_static(galaxy::HEADER_PROJECT_ID),
            HeaderName::from_static(galaxy::HEADER_GENERATION_ID),
            HeaderName::from_static(galaxy::HEADER_COMMIT_SHA),
            HeaderName::from_static(galaxy::HEADER_ARTIFACT_VERSION),
            HeaderName::from_static(galaxy::HEADER_SEMANTIC_HASH),
        ])
        .allow_credentials(true)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    database: crate::db::runtime::DatabaseRuntimeHealth,
    provider_catalog: ProviderCatalogHealth,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderCatalogSourceTier {
    Embedded,
    Live,
    Stale,
}

impl From<djinn_provider::catalog::SourceTier> for ProviderCatalogSourceTier {
    fn from(tier: djinn_provider::catalog::SourceTier) -> Self {
        match tier {
            djinn_provider::catalog::SourceTier::Embedded => Self::Embedded,
            djinn_provider::catalog::SourceTier::Live => Self::Live,
            djinn_provider::catalog::SourceTier::Stale => Self::Stale,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderCatalogRefreshStatus {
    Never,
    Success,
    Error,
}

impl From<djinn_provider::catalog::RefreshStatus> for ProviderCatalogRefreshStatus {
    fn from(status: djinn_provider::catalog::RefreshStatus) -> Self {
        match status {
            djinn_provider::catalog::RefreshStatus::Never => Self::Never,
            djinn_provider::catalog::RefreshStatus::Success => Self::Success,
            djinn_provider::catalog::RefreshStatus::Error => Self::Error,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ProviderCatalogHealth {
    source_tier: ProviderCatalogSourceTier,
    fetched_at: Option<String>,
    age_seconds: Option<u64>,
    refresh_interval_seconds: u64,
    last_refresh_status: ProviderCatalogRefreshStatus,
    last_refresh_error: Option<String>,
}

/// Format a `SystemTime` as RFC3339, degrading to `None` on out-of-range or
/// formatting errors so the health endpoint cannot panic.
fn format_system_time_rfc3339(ts: SystemTime) -> Option<String> {
    let offset = time::OffsetDateTime::from_unix_timestamp(
        i64::try_from(ts.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs()).ok()?,
    )
    .ok()?;
    offset.format(&Rfc3339).ok()
}

async fn health(State(state): State<AppState>) -> axum::Json<HealthResponse> {
    let interval_secs = state.provider_catalog_refresh_interval_secs();
    let max_age = Duration::from_secs(interval_secs.saturating_mul(2));
    let catalog = state.catalog();
    let age = catalog.last_successful_fetch_age();
    let source_tier: ProviderCatalogSourceTier = catalog.source_tier(max_age).into();
    let last_refresh_status: ProviderCatalogRefreshStatus = catalog.last_refresh_status().into();
    let last_refresh_error = catalog.last_refresh_error();
    let fetched_at = catalog
        .last_successful_fetch_time()
        .and_then(format_system_time_rfc3339);

    axum::Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        database: state.database_health(),
        provider_catalog: ProviderCatalogHealth {
            source_tier,
            fetched_at,
            age_seconds: age.map(|d| d.as_secs()),
            refresh_interval_seconds: interval_secs,
            last_refresh_status,
            last_refresh_error,
        },
    })
}

/// Run the server, blocking until shutdown signal.
///
/// After the cancellation token fires, the server waits up to 5 seconds for
/// in-flight connections to finish before returning.  This prevents the
/// process from hanging indefinitely on long-lived connections (SSE, MCP
/// streams) that didn't notice the shutdown signal.
pub async fn run(router: Router, port: u16, cancel: CancellationToken) {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind");

    tracing::info!(port, "listening on 0.0.0.0:{port}");

    // Clone the token so we can also use it for the deadline below.
    let shutdown_cancel = cancel.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(cancel.cancelled_owned());

    // Spawn the server so we can race it against a hard deadline.
    let handle = tokio::spawn(async move {
        if let Err(e) = server.await {
            tracing::error!(error = %e, "server error");
        }
    });

    // Wait for the shutdown signal, then give in-flight connections 5s.
    shutdown_cancel.cancelled().await;
    match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
        Ok(Ok(())) => tracing::info!("server shut down gracefully"),
        Ok(Err(e)) => tracing::warn!(error = %e, "server task panicked"),
        Err(_) => tracing::warn!("server shutdown timed out after 5s, forcing exit"),
    }
}

#[cfg(test)]
mod tests;
