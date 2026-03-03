use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use djinn_server::db::connection::Database;
use djinn_server::server::{self, AppState};

#[derive(Parser)]
#[command(name = "djinn-server", about = "Djinn MCP server")]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value_t = 8372, env = "DJINN_PORT")]
    port: u16,
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

    let db = Database::open_in_memory().expect("failed to open database");
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
