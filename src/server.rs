use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::db::connection::Database;
use crate::mcp;

/// Shared application state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pub db: Database,
    pub cancel: CancellationToken,
}

impl AppState {
    pub fn new(db: Database, cancel: CancellationToken) -> Self {
        Self {
            inner: Arc::new(Inner { db, cancel }),
        }
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.inner.cancel
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> axum::Json<HealthResponse> {
    axum::Json(HealthResponse { status: "ok" })
}

/// Build the application router.
pub fn router(state: AppState) -> Router {
    let mcp_service =
        mcp::server::DjinnMcpServer::into_service(state.clone(), state.cancel().clone());

    Router::new()
        .route("/health", get(health))
        .nest_service("/mcp", mcp_service)
        .with_state(state)
}

/// Run the server, blocking until shutdown signal.
pub async fn run(router: Router, port: u16, cancel: CancellationToken) {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind");

    tracing::info!(port, "listening on 0.0.0.0:{port}");

    axum::serve(listener, router)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .expect("server error");

    tracing::info!("server shut down");
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::test_helpers;

    /// Integration test: hit /health via tower::ServiceExt::oneshot().
    #[tokio::test]
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

    /// Unit test: verify the in-memory test DB has migrations applied.
    #[tokio::test]
    async fn test_db_has_tables() {
        let db = test_helpers::create_test_db();

        db.call(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 1, "settings table should exist");
            Ok(())
        })
        .await
        .unwrap();
    }

    /// Demonstrates tokio::test(start_paused = true) for time-dependent logic.
    /// With start_paused, tokio::time::sleep completes instantly (time is virtual).
    #[tokio::test(start_paused = true)]
    async fn time_paused_pattern() {
        let before = tokio::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let elapsed = before.elapsed();

        // With start_paused, the 60s sleep advances virtual time instantly.
        assert_eq!(elapsed.as_secs(), 60);
    }
}
