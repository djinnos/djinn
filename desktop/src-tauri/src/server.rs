use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio_util::sync::CancellationToken;

/// Runtime connection state managed by Tauri.
#[derive(Debug)]
pub struct ServerState {
    /// Full base URL of the connected server, e.g. `http://127.0.0.1:8372`.
    pub base_url: Option<String>,
    /// Port extracted from `base_url` — kept for backward-compatible commands.
    pub port: Option<u16>,
    pub ready: bool,
    pub is_healthy: bool,
    pub has_error: bool,
    pub error_message: Option<String>,
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            base_url: None,
            port: None,
            ready: false,
            is_healthy: false,
            has_error: false,
            error_message: None,
        }
    }

    pub fn mark_healthy(&mut self, base_url: &str) {
        self.port = parse_port(base_url);
        self.base_url = Some(base_url.to_string());
        self.is_healthy = true;
        self.has_error = false;
        self.error_message = None;
        self.ready = true;
    }

    pub fn mark_error(&mut self, message: &str) {
        self.is_healthy = false;
        self.has_error = true;
        self.error_message = Some(message.to_string());
        self.ready = false;
    }
}

pub fn init_server_state() -> ServerState {
    ServerState::new()
}

/// Start the djinn server embedded in-process and return the base URL.
///
/// If port 8372 is already in use (e.g. a dev server is running), falls back
/// to connecting to that existing server rather than returning an error.
pub async fn start_embedded(cancel: CancellationToken) -> Result<String, String> {
    let config = djinn_server::embedded::Config {
        port: 8372,
        db_path: None,
    };
    match djinn_server::embedded::start(config, cancel).await {
        Ok(port) => Ok(format!("http://127.0.0.1:{port}")),
        Err(e) => {
            // If the port is already in use, reuse the existing server.
            let fallback = "http://127.0.0.1:8372";
            if health_check(fallback).await {
                log::info!("Port in use; reusing existing server at {fallback}");
                Ok(fallback.to_string())
            } else {
                Err(e)
            }
        }
    }
}

/// Verify a remote server URL is reachable.
pub async fn check_remote(base_url: &str) -> bool {
    health_check(base_url).await
}

/// HTTP GET `{base_url}/health` — returns true if the server responds 2xx.
pub async fn health_check(base_url: &str) -> bool {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    match reqwest::get(&url).await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

/// Retry connecting to the server according to the current connection mode.
///
/// For embedded: restarts the embedded server using the managed cancel token.
/// For remote: re-runs the health check.
pub async fn retry_connection<R: Runtime>(app: &AppHandle<R>) -> Result<String, String> {
    // Clear existing error state
    if let Some(state) = app.try_state::<Mutex<ServerState>>() {
        if let Ok(mut s) = state.lock() {
            s.has_error = false;
            s.error_message = None;
        }
    }

    let mode = crate::connection_mode::load();
    let base_url = match mode {
        crate::connection_mode::ConnectionMode::Embedded => {
            // Create a fresh cancel token — the old embedded server (if any) keeps its own.
            let cancel = app
                .try_state::<CancellationToken>()
                .map(|t| t.inner().clone())
                .unwrap_or_else(CancellationToken::new);
            start_embedded(cancel).await?
        }
        crate::connection_mode::ConnectionMode::Remote { url } => {
            if health_check(&url).await {
                url
            } else {
                return Err(format!("Remote server at {url} is not reachable"));
            }
        }
    };

    if let Some(state) = app.try_state::<Mutex<ServerState>>() {
        if let Ok(mut s) = state.lock() {
            s.mark_healthy(&base_url);
        }
    }

    Ok(base_url)
}

/// Spawn a background task that periodically health-checks the server.
///
/// On failure it tries to re-discover (re-read `base_url` from state) and
/// emits `server:reconnected` or `server:disconnected` events so the frontend
/// can react.
pub fn start_health_monitor<R: Runtime>(app: &AppHandle<R>) {
    let app_handle = app.clone();

    tauri::async_runtime::spawn(async move {
        // Wait for initial startup to settle.
        tokio::time::sleep(Duration::from_secs(5)).await;

        let mut was_healthy = true;

        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;

            let current_url = {
                let state = app_handle.state::<Mutex<ServerState>>();
                state.lock().ok().and_then(|s| s.base_url.clone())
            };

            let Some(base_url) = current_url else {
                continue; // startup hasn't finished
            };

            if health_check(&base_url).await {
                if !was_healthy {
                    log::info!("Health monitor: server recovered at {base_url}");
                    if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                        if let Ok(mut s) = state.lock() {
                            s.mark_healthy(&base_url);
                        }
                    }
                    let _ = app_handle.emit("server:reconnected", &base_url);
                    was_healthy = true;
                }
                continue;
            }

            log::warn!("Health monitor: server at {base_url} is unreachable");

            if was_healthy {
                if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                    if let Ok(mut s) = state.lock() {
                        s.is_healthy = false;
                    }
                }
                let _ = app_handle.emit("server:disconnected", ());
                was_healthy = false;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort port extraction from a URL string.
fn parse_port(url: &str) -> Option<u16> {
    // Strip scheme, find last ':', take the part before any '/'
    let after_scheme = url.splitn(3, "//").nth(1).unwrap_or(url);
    let host_port = after_scheme.splitn(2, '/').next().unwrap_or(after_scheme);
    // host_port is either "host:port" or just "host"
    host_port.rsplit(':').next().and_then(|s| s.parse().ok())
}
