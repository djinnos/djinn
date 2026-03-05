use std::path::PathBuf;
use std::process::Stdio;

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
use tokio_util::sync::CancellationToken;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use djinn_server::daemon;
use djinn_server::db::checkpoint;
use djinn_server::db::connection::{self, Database};
use djinn_server::logging;
use djinn_server::server::{self, AppState};

const DEFAULT_UPSTREAM_URL: &str = "http://127.0.0.1:8372/mcp";

#[derive(Parser)]
#[command(name = "djinn-server", about = "Djinn MCP server")]
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
    #[arg(short, long, default_value_t = 8372, env = "DJINN_PORT")]
    port: u16,

    /// Database path (default: ~/.djinn/djinn.db)
    #[arg(long, env = "DJINN_DB_PATH")]
    db_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let _log_guards = init_logging();

    let cli = Cli::parse();

    if cli.ensure_daemon {
        if let Err(e) = ensure_daemon_running(&cli).await {
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

    let db_path = cli.db_path.unwrap_or_else(connection::default_db_path);
    tracing::info!(path = %db_path.display(), "opening database");
    let db = Database::open(&db_path).expect("failed to open database");
    checkpoint::spawn(db.clone(), cancel.clone());

    let state = AppState::new(db, cancel.clone());
    state.initialize().await;
    state.initialize_agents().await;
    let router = server::router(state);

    server::run(router, cli.port, cancel).await;
}

#[derive(Clone)]
struct StdioBridge {
    upstream: Peer<RoleClient>,
    upstream_url: String,
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
        self.upstream.list_tools(request).await.map_err(|e| {
            McpError::internal_error(format!("upstream list_tools failed: {e}"), None)
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.upstream.call_tool(request).await.map_err(|e| {
            McpError::internal_error(format!("upstream call_tool failed: {e}"), None)
        })
    }
}

async fn run_stdio_bridge(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    ensure_daemon_running(cli).await?;

    let daemon_info = read_daemon_info();
    let upstream_url = resolve_upstream_url(cli.mcp_url.clone(), daemon_info.as_ref());

    tracing::info!(url = %upstream_url, "starting stdio bridge");

    let config = StreamableHttpClientTransportConfig::with_uri(upstream_url.clone());
    let upstream_transport = StreamableHttpClientTransport::from_config(config);
    let upstream_service = ().serve(upstream_transport).await?;

    let bridge = StdioBridge {
        upstream: upstream_service.peer().clone(),
        upstream_url,
    };

    let stdio_service = bridge.serve(stdio()).await?;
    let _ = stdio_service.waiting().await?;
    Ok(())
}

async fn ensure_daemon_running(cli: &Cli) -> Result<(), String> {
    if let Some(info) = read_daemon_info()
        && daemon::pid_is_alive(info.pid)
    {
        tracing::info!(pid = info.pid, port = info.port, "daemon already running");
        return Ok(());
    }

    let exe = std::env::current_exe().map_err(|e| format!("resolve current exe: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--port").arg(cli.port.to_string());
    if let Some(path) = &cli.db_path {
        cmd.arg("--db-path").arg(path);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn daemon process: {e}"))?;

    for _ in 0..40 {
        if let Some(info) = read_daemon_info()
            && daemon::pid_is_alive(info.pid)
        {
            tracing::info!(pid = info.pid, port = info.port, "daemon started");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    match child.try_wait() {
        Ok(Some(status)) => Err(format!("daemon process exited early: {status}")),
        Ok(None) => Err("daemon did not become healthy in time".to_string()),
        Err(e) => Err(format!("check daemon process status: {e}")),
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
    let path = daemon::daemon_file_path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<daemon::DaemonInfo>(&raw).ok()
}

fn init_logging() -> (WorkerGuard, WorkerGuard) {
    logging::setup_log_dir_and_retention();

    let file_appender =
        tracing_appender::rolling::daily(logging::logs_dir(), logging::file_prefix());
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);
    let (stderr_writer, stderr_guard) = tracing_appender::non_blocking(std::io::stderr());

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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
