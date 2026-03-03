use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use crate::sse;
use serde::Serialize;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use crate::actors::git::{GitActorHandle, GitError};
use crate::auth::JwksCache;
use crate::db::connection::Database;
use crate::events::DjinnEvent;
use crate::mcp;
use crate::provider::{CatalogService, HealthTracker};
use crate::sync::SyncManager;

const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Shared application state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pub db: Database,
    pub cancel: CancellationToken,
    pub events: broadcast::Sender<DjinnEvent>,
    pub git_actors: Mutex<HashMap<PathBuf, GitActorHandle>>,
    /// Clerk JWKS cache. `None` means auth is disabled (e.g. in tests).
    pub jwks: Option<JwksCache>,
    /// Clerk user ID from the startup token (AUTH-03).
    pub user_id: Option<String>,
    /// models.dev catalog + custom providers (in-memory, refreshed on startup).
    pub catalog: CatalogService,
    /// Per-model circuit-breaker health tracker.
    pub health_tracker: HealthTracker,
    /// djinn/ namespace git sync manager.
    pub sync: SyncManager,
}

impl AppState {
    /// Create an AppState without authentication (for tests / dev mode).
    pub fn new(db: Database, cancel: CancellationToken) -> Self {
        Self::new_inner(db, cancel, None, None)
    }

    /// Create an AppState with Clerk JWT auth enabled.
    pub fn new_with_auth(
        db: Database,
        cancel: CancellationToken,
        jwks: JwksCache,
        user_id: String,
    ) -> Self {
        Self::new_inner(db, cancel, Some(jwks), Some(user_id))
    }

    fn new_inner(
        db: Database,
        cancel: CancellationToken,
        jwks: Option<JwksCache>,
        user_id: Option<String>,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let sync = SyncManager::new(db.clone(), events.clone());
        Self {
            inner: Arc::new(Inner {
                db,
                cancel,
                events,
                git_actors: Mutex::new(HashMap::new()),
                jwks,
                user_id,
                catalog: CatalogService::new(),
                health_tracker: HealthTracker::new(),
                sync,
            }),
        }
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.inner.cancel
    }

    pub fn events(&self) -> &broadcast::Sender<DjinnEvent> {
        &self.inner.events
    }

    /// JWKS cache for Clerk JWT validation, or `None` if auth is disabled.
    pub fn jwks(&self) -> Option<&JwksCache> {
        self.inner.jwks.as_ref()
    }

    /// Clerk user ID established at startup, or `None` if auth is disabled.
    pub fn user_id(&self) -> Option<&str> {
        self.inner.user_id.as_deref()
    }

    /// Get or spawn a `GitActorHandle` for the given project path (GIT-04).
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.inner.git_actors.lock().await;
        crate::actors::git::get_or_spawn(&mut map, path)
    }

    pub fn catalog(&self) -> &CatalogService {
        &self.inner.catalog
    }

    pub fn health_tracker(&self) -> &HealthTracker {
        &self.inner.health_tracker
    }

    pub fn sync_manager(&self) -> &SyncManager {
        &self.inner.sync
    }

    /// Load custom providers from DB into the catalog and trigger a background
    /// catalog refresh from models.dev.  Call once after server startup.
    pub async fn initialize(&self) {
        use crate::db::repositories::custom_provider::CustomProviderRepository;
        use crate::models::provider::{Model, Provider};

        // Load custom providers from DB → merge into in-memory catalog.
        let repo = CustomProviderRepository::new(self.db().clone());
        match repo.list().await {
            Ok(providers) => {
                for cp in providers {
                    let provider = Provider {
                        id: cp.id.clone(),
                        name: cp.name,
                        npm: String::new(),
                        env_vars: vec![cp.env_var],
                        base_url: cp.base_url,
                        docs_url: String::new(),
                        is_openai_compatible: true,
                    };
                    let seed_models: Vec<Model> = cp
                        .seed_models
                        .iter()
                        .map(|s| Model {
                            id: s.id.clone(),
                            provider_id: cp.id.clone(),
                            name: s.name.clone(),
                            tool_call: false,
                            reasoning: false,
                            attachment: false,
                            context_window: 0,
                            output_limit: 0,
                            pricing: crate::models::provider::Pricing::default(),
                        })
                        .collect();
                    self.catalog().add_custom_provider(provider, seed_models);
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to load custom providers from DB"),
        }

        // Kick off background refresh from models.dev.
        let catalog = self.catalog().clone();
        tokio::spawn(async move {
            catalog.refresh().await;
        });

        // Restore sync state from DB and start background auto-export task.
        let sync = self.sync_manager().clone();
        sync.restore().await;
        let uid = self.user_id().unwrap_or("local").to_string();
        sync.spawn_background_task(self.cancel().clone(), uid);
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

    // Apply JWT auth middleware to /mcp only. The middleware is a no-op when
    // auth is not configured (AppState::jwks() returns None).
    // Use .layer() because fallback_service is not a named route so route_layer() is a no-op.
    let mcp_router = Router::new()
        .fallback_service(mcp_service)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::middleware::require_auth,
        ));

    Router::new()
        .route("/health", get(health))
        .route("/events", get(sse::events_handler))
        .route("/db-info", get(sse::db_info_handler))
        .nest("/mcp", mcp_router)
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
        db.ensure_initialized().await.unwrap();

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "settings table should exist");
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
