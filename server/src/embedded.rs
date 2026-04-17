/// Embedded server startup — runs the djinn server in-process as a background
/// tokio task instead of a separate sidecar binary.
///
/// Designed to be called from the desktop app. The caller supplies a
/// `CancellationToken`; cancelling it triggers the server's graceful shutdown
/// (up to 5 seconds of connection draining).
use std::path::PathBuf;

use djinn_db::{Database, DatabaseConnectConfig, MysqlBackendFlavor, MysqlDatabaseConfig};
use tokio_util::sync::CancellationToken;

/// Configuration for the embedded server.
pub struct Config {
    /// Port to listen on. Use `0` to let the OS pick a free port.
    pub port: u16,
    /// Legacy database path (retained for call-site compatibility). The
    /// embedded server now always targets the compose-managed Dolt backend;
    /// this field is ignored.
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
    let _ = config.db_path; // kept for API stability; Dolt URL is environment-driven
    let url = std::env::var("DJINN_MYSQL_URL")
        .unwrap_or_else(|_| "mysql://root@127.0.0.1:3306/djinn".to_owned());
    tracing::info!(url = %url, "embedded: opening database");
    let db = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
        url,
        flavor: MysqlBackendFlavor::Dolt,
    }))
    .map_err(|e| format!("open database: {e}"))?;

    let state = crate::server::AppState::new(db, cancel.clone());
    state.initialize().await;
    state.initialize_agents().await;

    // Hold a handle on AppState so we can tear the RPC TCP listener down
    // cleanly after the HTTP server's shutdown future resolves.
    let state_for_shutdown = state.clone();
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
        // Graceful RPC teardown — matches the `djinn-server` binary's shutdown
        // sequence in `main::async_main`.
        state_for_shutdown.shutdown_rpc_listener().await;
        tracing::info!("embedded: server stopped");
    });

    Ok(port)
}
