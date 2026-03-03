use std::path::PathBuf;

use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use djinn_server::auth::JwksCache;
use djinn_server::db::checkpoint;
use djinn_server::db::connection::{self, Database};
use djinn_server::server::{self, AppState};

/// Default Clerk JWKS endpoint (can be overridden via DJINN_CLERK_JWKS_URL).
const DEFAULT_JWKS_URL: &str = "https://api.clerk.com/v1/jwks";

#[derive(Parser)]
#[command(name = "djinn-server", about = "Djinn MCP server")]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value_t = 8372, env = "DJINN_PORT")]
    port: u16,

    /// Database path (default: ~/.djinn/djinn.db)
    #[arg(long, env = "DJINN_DB_PATH")]
    db_path: Option<PathBuf>,

    /// Clerk JWT for authentication. Required in production; omit to disable auth.
    #[arg(long, env = "DJINN_TOKEN")]
    token: Option<String>,

    /// Clerk JWKS URL (default: https://api.clerk.com/v1/jwks)
    #[arg(long, env = "DJINN_CLERK_JWKS_URL", default_value = DEFAULT_JWKS_URL)]
    clerk_jwks_url: String,
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

    let state = build_state(
        db,
        cancel.clone(),
        cli.token.as_deref(),
        &cli.clerk_jwks_url,
    )
    .await;
    state.initialize().await;
    state.initialize_agents().await;
    let router = server::router(state);

    server::run(router, cli.port, cancel).await;
}

/// Build AppState, validating the startup token if provided (AUTH-01, AUTH-03, AUTH-04).
async fn build_state(
    db: Database,
    cancel: CancellationToken,
    token: Option<&str>,
    jwks_url: &str,
) -> AppState {
    let Some(token) = token else {
        tracing::warn!("no DJINN_TOKEN provided — authentication disabled");
        return AppState::new(db, cancel);
    };

    let jwks = JwksCache::new(jwks_url);
    let claims = jwks.validate(token).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "startup token validation failed — refusing to start");
        std::process::exit(1);
    });

    tracing::info!(user_id = %claims.sub, "authenticated as Clerk user");
    AppState::new_with_auth(db, cancel, jwks, claims.sub)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
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
