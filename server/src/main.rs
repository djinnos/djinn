use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use djinn_agent::runtime_bridge::{RuntimeKind, runtime_kind};
use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};
use djinn_core::doctor::RetrievalHealthConfig;
use djinn_server::db::runtime::{DatabaseRuntimeConfig, DatabaseRuntimeManager};
use djinn_server::logging;
use djinn_server::server::{self, AppState};
use djinn_telemetry::memory_retrieval::MemoryRetrievalMetrics;

#[derive(Parser)]
#[command(name = "djinn-server", about = "Djinn MCP server", version)]
struct Cli {
    /// Port to listen on
    #[arg(short, long, default_value_t = 3000, env = "DJINN_PORT")]
    port: u16,

    /// Legacy on-disk DB path. Retained as a CLI shim for backward
    /// compatibility; the active runtime is a sibling Postgres service.
    #[arg(long, env = "DJINN_DB_PATH")]
    db_path: Option<PathBuf>,

    /// Postgres DSN, e.g. `postgres://user:pass@host:5432/dbname`.
    #[arg(long, env = "DJINN_DATABASE_URL")]
    database_url: Option<String>,

    /// Serve the embedded Vite-built React UI as the HTTP fallback.
    /// Disable for headless API-only deployments.
    #[arg(long, env = "DJINN_UI_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    ui_enabled: bool,

    /// One-shot mode: apply every pending SQL migration, then exit.
    ///
    /// Run by the server Deployment's `migrate` initContainer so the app
    /// container can skip the migrator lock entirely on boot — it just
    /// verifies the schema matches what its binary expects. The sqlx
    /// migrator advisory lock serialises any overlapping migrator, so
    /// even on a rolling upgrade (maxSurge=1 → one new pod at a time)
    /// only one process applies migrations while the rest verify. Also
    /// useful standalone for debugging: `djinn-server --migrate-only`.
    #[arg(long, env = "DJINN_MIGRATE_ONLY", default_value_t = false)]
    migrate_only: bool,
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
    if let Err(e) = djinn_telemetry::init() {
        tracing::warn!(error = %e, "failed to initialize Prometheus telemetry");
    }

    let cli = Cli::parse();

    let cancel = CancellationToken::new();

    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received");
        shutdown_cancel.cancel();
    });

    let db_runtime = DatabaseRuntimeManager::new(
        DatabaseRuntimeConfig::from_cli_and_env(cli.db_path.clone(), cli.database_url.clone())
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

    // Under compose / Helm, postgres resolves to a service name like
    // `djinn-postgres:5432`. Wait for the DB port to accept connections
    // before the pool makes its first attempt — this keeps bootstrap
    // straightforward even when Postgres is slower to start than the
    // server container.
    wait_for_database_reachable(&startup_mode.target);
    db_runtime.ensure_runtime_available().unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to ensure database runtime availability");
        std::process::exit(1);
    });
    let db = db_runtime.bootstrap().unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to open database runtime");
        std::process::exit(1);
    });

    // ── Migrate-only short-circuit ────────────────────────────────────
    // The Helm `pre-upgrade` Job invokes us with this flag: apply every
    // pending migration, then exit. No HTTP listener, no actors, no
    // background tasks — the Job owns the DB exclusively for the
    // duration, which is the only safe place to run long ALTERs without
    // racing the app pod's connections.
    if cli.migrate_only {
        tracing::info!("migrate-only mode: applying pending migrations");
        if let Err(e) = db.ensure_initialized().await {
            tracing::error!(
                error = %e,
                "migrate-only: schema migration failed",
            );
            // Drain the pool even on failure so the server releases its
            // locks before the Job retries — otherwise the next retry hits
            // the same wedge we're trying to prevent.
            db.pool().close().await;
            std::process::exit(1);
        }
        tracing::info!("migrate-only mode: all migrations applied, shutting down");
        db.pool().close().await;
        return;
    }

    // App-pod boot path: schema must already be current (the pre-upgrade
    // Job applied it). We only verify — no `GET_LOCK`, no writes — so
    // even if a previous pod left a ghost lock on the migrator, this
    // path doesn't care.
    if let Err(e) = db.verify_schema_is_current().await {
        tracing::error!(
            error = %e,
            "schema verification failed — refusing to boot. Either the \
             pre-upgrade migrate Job has not run yet, or this binary \
             ships migrations the live DB has not received. Run \
             `djinn-server --migrate-only` to catch up before retrying."
        );
        db.pool().close().await;
        std::process::exit(1);
    }

    // ── Retrieval health config ───────────────────────────────────────
    // Parse once at process startup; invalid values prevent serving.
    let retrieval_config = RetrievalHealthConfig::from_env().unwrap_or_else(|e| {
        tracing::error!(error = %e, "invalid retrieval health configuration");
        std::process::exit(1);
    });
    let retrieval_metrics = Arc::new(MemoryRetrievalMetrics::new());

    let state = AppState::new_with_runtime(
        db,
        db_runtime,
        cancel.clone(),
        retrieval_config,
        retrieval_metrics,
    )
    .unwrap_or_else(|e| {
        tracing::error!(error = %e, "invalid build admission configuration");
        std::process::exit(1);
    });

    // NOTE: the periodic housekeeping + mirror-fetch loops, the coordinator,
    // and the worker RPC listener no longer start here unconditionally — they
    // run on the single lock-holding pod via `AppState::become_leader()`, which
    // the leadership manager (below) invokes once this process wins the
    // coordinator advisory lock. See `djinn_server::leadership`.

    // OTLP telemetry is configured at deploy time via env (set by the Helm
    // chart from values.langfuse.*). Absent env → telemetry stays off.
    if let Some(config) = djinn_provider::provider::telemetry::LangfuseConfig::from_env()
        && let Err(e) = djinn_provider::provider::telemetry::init(&config)
    {
        tracing::warn!(error = %e, "failed to initialize Langfuse telemetry");
    }

    state.init_app_config().await;
    state.initialize().await;

    // ── Leadership ────────────────────────────────────────────────────
    // Spawn the leadership manager: it races for the coordinator advisory
    // lock and, on winning, calls `become_leader()` to start the coordinator,
    // the worker RPC listener, and the other singleton/mutating subsystems.
    // The HTTP server (started below) serves on EVERY pod immediately —
    // readiness is deliberately independent of leadership so a
    // RollingUpdate(maxSurge=1, maxUnavailable=0) deploy never drops to zero
    // Ready endpoints (no more 503 window). The new pod serves HTTP in
    // standby until the old pod terminates, releases the lock, and the new
    // pod promotes itself.
    //
    // The lock is only engaged under the Kubernetes runtime (where multiple
    // pods can coexist). In-process / dev / test runs pass `None` and become
    // leader immediately, so they don't depend on a reachable Postgres for the
    // lock handshake.
    let lock_dsn = if matches!(runtime_kind(), RuntimeKind::Kubernetes) {
        cli.database_url.clone()
    } else {
        None
    };
    let leader_state = state.clone();
    let leader_cancel = cancel.clone();
    tokio::spawn(async move {
        djinn_server::leadership::run_with_leadership(
            lock_dsn,
            leader_cancel,
            move || async move {
                leader_state.become_leader().await;
            },
        )
        .await;
    });

    // Keep a handle on AppState so we can tear the TCP RPC listener down
    // gracefully after the HTTP server's shutdown future resolves.
    // `server::router(...)` moves the state into the router.
    let state_for_shutdown = state.clone();
    let router = server::router(state, cli.ui_enabled);

    server::run(router, cli.port, cancel).await;

    // Graceful shutdown: block new Enforce build-admission reservations
    // before draining in-flight work. Enforce admission begins draining so
    // no new task/warm create can start during the teardown window; in-flight
    // permits may still transition to terminal. Observe/Off are unaffected.
    state_for_shutdown.begin_build_admission_draining().await;

    // Graceful RPC teardown: cancel the TCP accept loop + join it so any
    // in-flight worker RPCs drain cleanly.  Matches the HTTP server's
    // `with_graceful_shutdown` semantics.
    state_for_shutdown.shutdown_rpc_listener().await;

    // Drain the SQLx pool BEFORE the process exits. `pool.close()` waits
    // for in-flight queries to finish and shuts each connection down via
    // the Postgres protocol — the server sees the close and releases
    // locks immediately. Capped at 30s so we never overrun
    // `terminationGracePeriodSeconds` and get SIGKILLed mid-drain.
    let pool = state_for_shutdown.db().pool().clone();
    match tokio::time::timeout(std::time::Duration::from_secs(30), pool.close()).await {
        Ok(()) => tracing::info!("DB pool drained cleanly"),
        Err(_) => tracing::warn!(
            "DB pool drain timed out after 30s — postgres may hold locks until idle GC catches up"
        ),
    }

    // Flush any pending OTel spans before exit.
    djinn_provider::provider::telemetry::shutdown();
}

/// Parse a postgres URL of the form `postgres://<user>[:<pw>]@<host>:<port>/<db>`
/// and block until a TCP connection to host:port succeeds (up to ~60s).
fn wait_for_database_reachable(target: &str) {
    let host_port = target
        .strip_prefix("postgres://")
        .or_else(|| target.strip_prefix("postgresql://"))
        .and_then(|rest| rest.rsplit('@').next())
        .and_then(|after_at| after_at.split('/').next())
        .map(|s| {
            // If no port was specified after the host, append the default.
            if s.contains(':') {
                s.to_owned()
            } else {
                format!("{s}:5432")
            }
        })
        .unwrap_or_else(|| "postgres:5432".to_owned());
    let addr = host_port;
    tracing::info!(endpoint = %addr, "waiting for external database to accept TCP connections");

    let clock = SystemClockTrait::new();
    let deadline = clock.now_instant() + std::time::Duration::from_secs(60);
    while clock.now_instant() < deadline {
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

    // Default to JSON for prod log aggregation; allow `compact` for local-dev.
    // Honors `DJINN_LOG_FORMAT=json|compact` (anything else falls back to json).
    let log_format = std::env::var("DJINN_LOG_FORMAT")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "json".to_owned());

    let json_mode = log_format != "compact";

    if json_mode {
        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(file_writer);
        let stderr_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(stderr_writer);
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .with(stderr_layer)
            .init();
    } else {
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
    }

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
