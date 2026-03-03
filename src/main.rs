use std::path::PathBuf;

use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use djinn_server::db::checkpoint;
use djinn_server::db::connection::{self, Database};
use djinn_server::server::{self, AppState};

#[derive(Parser)]
#[command(name = "djinn-server", about = "Djinn MCP server")]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value_t = 8372, env = "DJINN_PORT")]
    port: u16,

    /// Database path (default: ~/.djinn/djinn.db)
    #[arg(long, env = "DJINN_DB_PATH")]
    db_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let cancel = CancellationToken::new();

    // Spawn shutdown signal handler.
    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received");
        shutdown_cancel.cancel();
    });

    let db_path = cli.db_path.unwrap_or_else(connection::default_db_path);
    tracing::info!(path = %db_path.display(), "opening database");
    let db = Database::open(&db_path).expect("failed to open database");
    checkpoint::spawn(db.clone(), cancel.clone());
    let state = AppState::new(db, cancel.clone());
    let router = server::router(state);

    server::run(router, cli.port, cancel).await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
