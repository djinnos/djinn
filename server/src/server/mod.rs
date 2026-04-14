use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;

use crate::sse;

mod agents;
mod chat;
mod mcp_handler;
mod project_tools;
mod state;
pub use state::AppState;

/// Build the application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/events", get(sse::events_handler))
        .route("/db-info", get(sse::db_info_handler))
        .route("/api/chat/completions", post(chat::completions_handler))
        .route("/mcp", post(mcp_handler::mcp_handler))
        .merge(agents::router())
        .merge(project_tools::router())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    database: crate::db::runtime::DatabaseRuntimeHealth,
}

async fn health(State(state): State<AppState>) -> axum::Json<HealthResponse> {
    axum::Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        database: state.database_health(),
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
