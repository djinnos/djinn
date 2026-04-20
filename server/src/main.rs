use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::{Peer, RequestContext, RoleClient, RoleServer},
    transport::{
        StreamableHttpClientTransport, io::stdio,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use tokio::signal;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use djinn_server::daemon;
use djinn_server::db::runtime::{DatabaseRuntimeConfig, DatabaseRuntimeManager};
use djinn_server::logging;
use djinn_server::server::{self, AppState};

const DEFAULT_UPSTREAM_URL: &str = "http://127.0.0.1:3000/mcp";

#[derive(Parser)]
#[command(name = "djinn-server", about = "Djinn MCP server", version)]
struct Cli {
    /// Run as stdio bridge to daemon HTTP MCP endpoint.
    #[arg(long, default_value_t = false)]
    mcp_connect: bool,

    /// Ensure daemon is running and exit.
    #[arg(long, default_value_t = false)]
    ensure_daemon: bool,

    /// Upstream MCP URL override for --mcp-connect.
    #[arg(long, env = "DJINN_MCP_URL")]
    mcp_url: Option<String>,

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

    if cli.ensure_daemon {
        if let Err(e) = ensure_daemon_running(cli.port, cli.db_path.as_deref()).await {
            tracing::error!(error = %e, "failed to ensure daemon is running");
            std::process::exit(1);
        }
        return;
    }

    if cli.mcp_connect {
        if let Err(e) = run_stdio_bridge(&cli).await {
            tracing::error!(error = %e, "stdio bridge failed");
            std::process::exit(1);
        }
        return;
    }

    let cancel = CancellationToken::new();
    let _daemon_lock = daemon::acquire(cli.port).unwrap_or_else(|e| {
        tracing::error!(error = %e, "failed to acquire daemon lockfile");
        std::process::exit(1);
    });

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

    let state = AppState::new_with_runtime(db, db_runtime, cancel.clone());
    djinn_server::housekeeping::spawn(state.clone());
    djinn_server::mirror_fetcher::spawn(state.clone());

    // OTLP telemetry is configured at deploy time via env (set by the Helm
    // chart from values.langfuse.*). Absent env → telemetry stays off.
    if let Some(config) = djinn_agent::provider::telemetry::LangfuseConfig::from_env()
        && let Err(e) = djinn_agent::provider::telemetry::init(&config)
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
    // `server::router(state)` moves the state into the router.
    let state_for_shutdown = state.clone();
    let router = server::router(state);

    server::run(router, cli.port, cancel).await;

    // Graceful RPC teardown: cancel the TCP accept loop + join it so any
    // in-flight worker RPCs drain cleanly.  Matches the HTTP server's
    // `with_graceful_shutdown` semantics.
    state_for_shutdown.shutdown_rpc_listener().await;

    // Flush any pending OTel spans before exit.
    djinn_agent::provider::telemetry::shutdown();
}

#[derive(Clone)]
struct StdioBridge {
    upstream: Arc<Mutex<Peer<RoleClient>>>,
    upstream_url: String,
    port: u16,
    db_path: Option<PathBuf>,
}

impl StdioBridge {
    async fn connect(url: &str) -> Result<Peer<RoleClient>, Box<dyn std::error::Error>> {
        let config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
        let transport = StreamableHttpClientTransport::from_config(config);
        let service = ().serve(transport).await?;
        let peer = service.peer().clone();
        // Keep the upstream service alive in the background
        tokio::spawn(async move {
            let _ = service.waiting().await;
        });
        Ok(peer)
    }

    async fn reconnect(&self) -> Result<(), McpError> {
        tracing::info!("upstream connection lost, attempting reconnect");
        ensure_daemon_running(self.port, self.db_path.as_deref())
            .await
            .map_err(|e| McpError::internal_error(format!("ensure daemon: {e}"), None))?;

        let peer = Self::connect(&self.upstream_url)
            .await
            .map_err(|e| McpError::internal_error(format!("reconnect failed: {e}"), None))?;

        *self.upstream.lock().await = peer;
        tracing::info!("upstream reconnected successfully");
        Ok(())
    }
}

impl ServerHandler for StdioBridge {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "djinn-server-bridge".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(format!("Forwards MCP tools to {}", self.upstream_url)),
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let peer = self.upstream.lock().await.clone();
        match peer.list_tools(request.clone()).await {
            Ok(result) => Ok(result),
            Err(first_err) => {
                tracing::warn!(error = %first_err, "upstream list_tools failed, reconnecting");
                self.reconnect().await?;
                let peer = self.upstream.lock().await.clone();
                peer.list_tools(request).await.map_err(|e| {
                    McpError::internal_error(
                        format!("upstream list_tools failed after reconnect: {e}"),
                        None,
                    )
                })
            }
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let peer = self.upstream.lock().await.clone();
        match peer.call_tool(request.clone()).await {
            Ok(result) => Ok(result),
            Err(first_err) => {
                tracing::warn!(error = %first_err, "upstream call_tool failed, reconnecting");
                self.reconnect().await?;
                let peer = self.upstream.lock().await.clone();
                peer.call_tool(request).await.map_err(|e| {
                    McpError::internal_error(
                        format!("upstream call_tool failed after reconnect: {e}"),
                        None,
                    )
                })
            }
        }
    }
}

async fn run_stdio_bridge(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    ensure_daemon_running(cli.port, cli.db_path.as_deref()).await?;

    let daemon_info = read_daemon_info();
    let upstream_url = resolve_upstream_url(cli.mcp_url.clone(), daemon_info.as_ref());

    tracing::info!(url = %upstream_url, "starting stdio bridge");

    let peer = StdioBridge::connect(&upstream_url).await?;

    let bridge = StdioBridge {
        upstream: Arc::new(Mutex::new(peer)),
        upstream_url,
        port: cli.port,
        db_path: cli.db_path.clone(),
    };

    let stdio_service = bridge.serve(stdio()).await?;
    let _ = stdio_service.waiting().await?;
    Ok(())
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

async fn ensure_daemon_running(port: u16, db_path: Option<&std::path::Path>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("resolve current exe: {e}"))?;
    // On Linux, /proc/self/exe resolves with a trailing " (deleted)" suffix
    // when the backing binary has been replaced on disk (e.g. after a cargo
    // rebuild while the stdio bridge is still running). `current_exe()`
    // returns that literal string, and handing it to `Command::new` fails
    // with ENOENT. Strip the suffix so we exec the freshly-rebuilt file at
    // the original path instead.
    let exe = strip_deleted_suffix(exe);
    daemon::ensure_running(port, db_path, &exe).await?;
    Ok(())
}

/// Strip the trailing `" (deleted)"` marker that Linux procfs appends to a
/// `/proc/self/exe` symlink whose target inode has been unlinked.
fn strip_deleted_suffix(path: std::path::PathBuf) -> std::path::PathBuf {
    const SUFFIX: &str = " (deleted)";
    match path.to_str() {
        Some(s) if s.ends_with(SUFFIX) => std::path::PathBuf::from(&s[..s.len() - SUFFIX.len()]),
        _ => path,
    }
}

fn resolve_upstream_url(
    override_url: Option<String>,
    daemon_info: Option<&daemon::DaemonInfo>,
) -> String {
    if let Some(url) = override_url {
        return url;
    }
    if let Some(info) = daemon_info {
        return format!("http://127.0.0.1:{}/mcp", info.port);
    }
    DEFAULT_UPSTREAM_URL.to_string()
}

fn read_daemon_info() -> Option<daemon::DaemonInfo> {
    daemon::read_info_default()
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

#[cfg(test)]
mod strip_deleted_suffix_tests {
    use super::strip_deleted_suffix;
    use std::path::PathBuf;

    #[test]
    fn strips_trailing_deleted_marker() {
        let input = PathBuf::from("/usr/bin/foo (deleted)");
        assert_eq!(strip_deleted_suffix(input), PathBuf::from("/usr/bin/foo"));
    }

    #[test]
    fn leaves_unrelated_paths_alone() {
        let input = PathBuf::from("/usr/bin/foo");
        assert_eq!(strip_deleted_suffix(input), PathBuf::from("/usr/bin/foo"));
    }

    #[test]
    fn does_not_strip_deleted_inside_path() {
        // "(deleted) " in the middle is a legitimate filename component.
        let input = PathBuf::from("/weird/ (deleted) /bin");
        assert_eq!(
            strip_deleted_suffix(input),
            PathBuf::from("/weird/ (deleted) /bin")
        );
    }
}
