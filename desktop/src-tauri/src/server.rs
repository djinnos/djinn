use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, Runtime};

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

const DEFAULT_PORT: u16 = 8372;

/// Ensure the djinn daemon is running and return the base URL.
///
/// Checks `~/.djinn/daemon.json` for an existing daemon; if none is found
/// (or the recorded PID is dead), spawns a new `djinn-server` process that
/// detaches into its own session. The daemon survives desktop restarts.
pub async fn ensure_daemon() -> Result<String, String> {
    let server_bin = resolve_server_binary()?;
    let info =
        djinn_server::daemon::ensure_running(DEFAULT_PORT, None, &server_bin).await?;
    let base_url = format!("http://127.0.0.1:{}", info.port);

    // Wait for the HTTP health endpoint to be ready (the daemon may still be
    // initialising even after it writes daemon.json).
    for _ in 0..40 {
        if health_check(&base_url).await {
            return Ok(base_url);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Ok(base_url)
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
        crate::connection_mode::ConnectionMode::Daemon => ensure_daemon().await?,
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

/// Resolve the `djinn-server` binary path.
///
/// Search order:
/// 1. `DJINN_SERVER_BIN` environment variable
/// 2. Tauri sidecar path (for bundled production builds)
/// 3. `djinn-server` on `PATH`
fn resolve_server_binary() -> Result<PathBuf, String> {
    // 1. Explicit override via env var.
    if let Ok(path) = std::env::var("DJINN_SERVER_BIN") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!(
            "DJINN_SERVER_BIN is set to {path} but that file does not exist"
        ));
    }

    // 2. Tauri sidecar location (production builds).
    if let Some(path) = resolve_sidecar_path() {
        return Ok(path);
    }

    // 3. Co-located binary in the same Cargo target directory (dev builds).
    //    During `tauri dev`, the desktop exe and djinn-server are both built
    //    into the same target/debug (or target/release) directory.
    if let Some(path) = resolve_colocated_binary() {
        return Ok(path);
    }

    // 4. Search PATH.
    if let Some(path) = find_in_path("djinn-server") {
        return Ok(path);
    }

    Err(
        "djinn-server binary not found. Set DJINN_SERVER_BIN, \
         place it in PATH, or build with `cargo build -p djinn-server`."
            .to_string(),
    )
}

/// Try to locate the sidecar binary next to the running executable.
///
/// Tauri bundles sidecars at `<exe_dir>/binaries/<name>-<target_triple>[.exe]`
/// on macOS (inside the .app bundle) and at `<exe_dir>/<name>-<target_triple>`
/// on Linux/Windows.
fn resolve_sidecar_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    let triple = target_triple();
    let name = format!("djinn-server-{triple}");

    // macOS .app bundle: Contents/MacOS/<exe> → Contents/Resources/binaries/
    #[cfg(target_os = "macos")]
    {
        let resources = exe_dir.parent()?.join("Resources").join("binaries");
        let candidate = resources.join(&name);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Flat layout (Linux / Windows / dev):
    let candidate = exe_dir.join("binaries").join(&name);
    if candidate.exists() {
        return Some(candidate);
    }

    None
}

fn target_triple() -> &'static str {
    env!("DJINN_TARGET_TRIPLE")
}

/// During `tauri dev`, both the desktop app and `djinn-server` are compiled
/// into the same Cargo target directory (e.g. `target/debug/`).  Look for the
/// server binary next to the running executable.
fn resolve_colocated_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let candidate = exe_dir.join("djinn-server");
    if candidate.is_file() {
        return Some(candidate);
    }
    None
}

/// Search `PATH` for a binary by name.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Best-effort port extraction from a URL string.
fn parse_port(url: &str) -> Option<u16> {
    let after_scheme = url.split("//").nth(1).unwrap_or(url);
    let host_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    host_port.rsplit(':').next().and_then(|s| s.parse().ok())
}
