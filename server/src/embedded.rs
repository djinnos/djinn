/// Embedded server startup — runs the djinn server in-process as a background
/// tokio task instead of a separate sidecar binary.
///
/// Designed to be called from the Tauri desktop app. The caller supplies a
/// `CancellationToken`; cancelling it triggers the server's graceful shutdown
/// (up to 5 seconds of connection draining).
use std::path::PathBuf;

use djinn_db::{Database, default_db_path};
use tokio_util::sync::CancellationToken;

/// Configuration for the embedded server.
pub struct Config {
    /// Port to listen on. Use `0` to let the OS pick a free port.
    pub port: u16,
    /// Database path. Defaults to `~/.djinn/djinn.db` when `None`.
    pub db_path: Option<PathBuf>,
}

/// Start the djinn HTTP server in-process.
///
/// Initialises the database, builds the Axum router, and spawns the serve
/// loop as a background `tokio` task. Returns the port the server is actually
/// listening on (useful when `config.port == 0`).
///
/// The server stops when `cancel` is cancelled.
pub async fn start(config: Config, cancel: CancellationToken) -> Result<u16, String> {
    let db_path = config.db_path.unwrap_or_else(default_db_path);
    tracing::info!(path = %db_path.display(), "embedded: opening database");
    let db = Database::open(&db_path).map_err(|e| format!("open database: {e}"))?;

    crate::db::checkpoint::spawn(db.clone(), cancel.clone());

    let state = crate::server::AppState::new(db, cancel.clone());
    state.initialize().await;
    state.initialize_agents().await;

    let app = crate::server::router(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port))
        .await
        .map_err(|e| format!("bind port {}: {e}", config.port))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?
        .port();

    tracing::info!(port, "embedded: server listening on 127.0.0.1:{port}");

    tokio::spawn(async move {
        let srv = axum::serve(listener, app).with_graceful_shutdown(cancel.cancelled_owned());
        if let Err(e) = srv.await {
            tracing::error!(error = %e, "embedded server error");
        }
        tracing::info!("embedded: server stopped");
    });

    Ok(port)
}
