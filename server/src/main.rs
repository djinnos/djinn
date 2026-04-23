use std::path::PathBuf;

use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use djinn_server::db::runtime::{DatabaseRuntimeConfig, DatabaseRuntimeManager};
use djinn_server::logging;
use djinn_server::server::{self, AppState};

#[derive(Parser)]
#[command(name = "djinn-server", about = "Djinn MCP server", version)]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value_t = 3000, env = "DJINN_PORT")]
    port: u16,

    /// Database path (default: ~/.djinn/djinn.db)
    #[arg(long, env = "DJINN_DB_PATH")]
    db_path: Option<PathBuf>,

    /// Database backend selector: `mysql` or `dolt` (default: `dolt`).
    #[arg(long, env = "DJINN_DB_BACKEND")]
    db_backend: Option<String>,

    /// MySQL-compatible DSN for the Dolt/MySQL runtime.
    #[arg(long, env = "DJINN_MYSQL_URL")]
    mysql_url: Option<String>,

    /// Flavor of the MySQL-compatible backend: mysql or dolt.
    #[arg(long, env = "DJINN_MYSQL_FLAVOR")]
    mysql_flavor: Option<String>,

    /// Serve the embedded Vite-built React UI as the HTTP fallback.
    /// Disable for headless API-only deployments.
    #[arg(long, env = "DJINN_UI_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    ui_enabled: bool,
}

fn main() {
    // rustls 0.23 requires an explicit process-level CryptoProvider before
    // any TLS use. Without this every reqwest HTTPS call (LLM providers,
    // GitHub App, OTLP exporter) panics on first invocation.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(async_main());
}

async fn async_main() {
    let _log_guards = init_logging();

    let cli = Cli::parse();

    let cancel = CancellationToken::new();

    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received");
        shutdown_cancel.cancel();
    });

    let db_runtime = DatabaseRuntimeManager::new(
        DatabaseRuntimeConfig::from_cli_and_env(
            cli.db_path.clone(),
            cli.db_backend.clone(),
            cli.mysql_url.clone(),
            cli.mysql_flavor.clone(),
        )
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "invalid database runtime configuration");
            std::process::exit(1);
        }),
    );
    let startup_mode = db_runtime.startup_mode();
    tracing::info!(
        backend = %startup_mode.backend_label,
        target = %startup_mode.target,
        managed_process = startup_mode.managed_process,
        "opening database runtime"
    );

    // Under compose, dolt resolves to a service name like `dolt:3306`. Wait
    // for the DB port to accept connections before the pool makes its first
    // attempt — this keeps bootstrap straightforward even when Dolt is
    // slower to start than the server container.
    wait_for_dolt_reachable(&startup_mode.target);
    db_runtime.ensure_runtime_available().unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to ensure database runtime availability");
        std::process::exit(1);
    });
    let db = db_runtime.bootstrap().unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to open database runtime");
        std::process::exit(1);
    });

    // Apply schema migrations eagerly. sqlx's Database::ensure_initialized
    // is lazy — without this, migration mismatches (e.g. the binary was
    // compiled before a migration the live DB has already applied) surface
    // as scattered warnings from every background task that hits the DB,
    // and the server limps along in a half-broken state until something
    // downstream finally crashes. Fail fast with a clear message.
    if let Err(e) = db.ensure_initialized().await {
        tracing::error!(
            error = %e,
            "schema migration failed — binary is likely out of date \
             (or a committed migration file was mutated). Refusing to \
             start in a half-migrated state."
        );
        std::process::exit(1);
    }

    let state = AppState::new_with_runtime(db, db_runtime, cancel.clone());
    djinn_db::background::housekeeping::spawn(
        state.db().clone(),
        state.event_bus(),
        state.cancel().clone(),
    );
    djinn_server::mirror_fetcher::spawn(state.clone());

    // OTLP telemetry is configured at deploy time via env (set by the Helm
    // chart from values.langfuse.*). Absent env → telemetry stays off.
    if let Some(config) = djinn_provider::provider::telemetry::LangfuseConfig::from_env()
        && let Err(e) = djinn_provider::provider::telemetry::init(&config)
    {
        tracing::warn!(error = %e, "failed to initialize Langfuse telemetry");
    }

    state.init_app_config().await;
    state.initialize().await;
    state
        .initialize_memory_mount_from_db()
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to initialize memory mount");
            std::process::exit(1);
        });
    state.initialize_agents().await;

    // Keep a handle on AppState so we can tear the TCP RPC listener down
    // gracefully after the HTTP server's shutdown future resolves.
    // `server::router(...)` moves the state into the router.
    let state_for_shutdown = state.clone();
    let router = server::router(state, cli.ui_enabled);

    server::run(router, cli.port, cancel).await;

    // Graceful RPC teardown: cancel the TCP accept loop + join it so any
    // in-flight worker RPCs drain cleanly.  Matches the HTTP server's
    // `with_graceful_shutdown` semantics.
    state_for_shutdown.shutdown_rpc_listener().await;

    // Flush any pending OTel spans before exit.
    djinn_provider::provider::telemetry::shutdown();
}

/// Parse a mysql URL of the form `mysql://<user>[:<pw>]@<host>:<port>/<db>` and
/// block until a TCP connection to host:port succeeds (up to ~60s).
fn wait_for_dolt_reachable(target: &str) {
    let host_port = target
        .strip_prefix("mysql://")
        .and_then(|rest| rest.rsplit('@').next())
        .and_then(|after_at| after_at.split('/').next())
        .unwrap_or("dolt:3306");
    let addr = host_port.to_owned();
    tracing::info!(endpoint = %addr, "waiting for external database to accept TCP connections");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        if let Ok(mut iter) = std::net::ToSocketAddrs::to_socket_addrs(&addr)
            && let Some(sock) = iter.next()
            && std::net::TcpStream::connect_timeout(&sock, std::time::Duration::from_millis(500))
                .is_ok()
        {
            tracing::info!(endpoint = %addr, "external database is reachable");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    tracing::warn!(
        endpoint = %addr,
        "external database did not accept TCP connections within 60s; continuing and letting the pool retry"
    );
}

fn init_logging() -> (WorkerGuard, WorkerGuard) {
    logging::setup_log_dir_and_retention();

    let file_appender =
        tracing_appender::rolling::daily(logging::logs_dir(), logging::file_prefix());
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);
    let (stderr_writer, stderr_guard) = tracing_appender::non_blocking(std::io::stderr());

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,opentelemetry_sdk=warn"));
    let format = tracing_subscriber::fmt::format()
        .compact()
        .with_target(true);

    let file_layer = tracing_subscriber::fmt::layer()
        .event_format(format.clone())
        .with_ansi(false)
        .with_writer(file_writer);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .event_format(format)
        .with_writer(stderr_writer);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    (file_guard, stderr_guard)
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
