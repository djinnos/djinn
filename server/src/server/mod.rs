use axum::Router;
use axum::extract::State;
use axum::routing::{get, post};

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};

use crate::sse;

mod agents;
mod auth;
mod chat;
mod github_install;
mod mcp_handler;
mod org_sync;
mod project_tools;
mod state;
mod static_ui;
pub use auth::{AuthenticatedUser, authenticate};
pub use org_sync::{SyncReport, start_org_member_sync, sync_once};
pub use state::AppState;

/// Build the application router.
///
/// `serve_ui` controls whether the embedded Vite SPA is served as the
/// router's fallback. Leave it on (the default) for a single-image
/// deployment; turn it off (via `--ui-enabled=false` / `DJINN_UI_ENABLED=0`)
/// for headless API-only deployments where the UI lives elsewhere.
pub fn router(state: AppState, serve_ui: bool) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/events", get(sse::events_handler))
        .route("/db-info", get(sse::db_info_handler))
        .route("/api/chat/completions", post(chat::completions_handler))
        .merge(chat::sessions::router())
        .route("/mcp", post(mcp_handler::mcp_handler))
        .merge(agents::router())
        .merge(auth::router())
        .merge(github_install::router())
        .merge(crate::mirror_fetcher::router())
        .merge(org_sync::router())
        .merge(project_tools::router());
    if serve_ui {
        router = router.fallback(static_ui::serve_static);
    }
    router.layer(cors_layer()).with_state(state)
}

/// CORS layer that allows any origin but — crucially — **reflects** the
/// request origin instead of returning `*`, so browsers accept responses
/// to `credentials: 'include'` requests (session cookie auth).
fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
        .allow_credentials(true)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    database: crate::db::runtime::DatabaseRuntimeHealth,
    memory_mount: MemoryMountHealth,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoryMountLifecycleState {
    Disabled,
    Configured,
    Mounted,
    Degraded,
}

/// Describes which mounted-memory view ADR-057 is currently serving.
///
/// Operators should read this alongside `fallback`: a `canonical` view with no
/// fallback means the mount is intentionally serving the canonical repository
/// view, while a populated fallback explains why task-scoped resolution could
/// not be used for the current filesystem-first session.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoryMountViewKind {
    Canonical,
    TaskScoped,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemoryMountViewFallbackReason {
    AmbiguousActiveTasks,
    ActiveTaskNotFound,
    TaskProjectMismatch,
    NoActiveSession,
    MissingSessionWorktree,
    CanonicalProjectRoot,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct MemoryMountViewFallback {
    pub(crate) reason: MemoryMountViewFallbackReason,
    pub(crate) detail: Option<String>,
    pub(crate) active_task_count: Option<usize>,
    pub(crate) task_id: Option<String>,
    pub(crate) task_short_id: Option<String>,
    pub(crate) task_project_id: Option<String>,
    pub(crate) mount_project_id: Option<String>,
    pub(crate) session_workspace_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct MemoryMountViewHealth {
    pub(crate) kind: MemoryMountViewKind,
    pub(crate) task_short_id: Option<String>,
    pub(crate) worktree_root: Option<String>,
    pub(crate) fallback: Option<MemoryMountViewFallback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryMountViewResolution {
    pub(crate) selection: crate::memory_fs::MemoryViewSelection,
    pub(crate) health: MemoryMountViewHealth,
}

#[derive(Serialize)]
pub(crate) struct MemoryMountHealth {
    enabled: bool,
    active: bool,
    lifecycle: MemoryMountLifecycleState,
    configured: bool,
    mount_path: Option<String>,
    project_id: Option<String>,
    detail: Option<String>,
    view: MemoryMountViewHealth,
    pending_writes: usize,
    last_error: Option<String>,
}

async fn health(State(state): State<AppState>) -> axum::Json<HealthResponse> {
    axum::Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        database: state.database_health(),
        memory_mount: state.memory_mount_health().await,
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
