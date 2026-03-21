use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

/// Daemon information written by the server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub pid: u32,
}

/// Server state managed by Tauri
#[derive(Debug)]
pub struct ServerState {
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
            port: None,
            ready: false,
            is_healthy: false,
            has_error: false,
            error_message: None,
        }
    }

    pub fn mark_healthy(&mut self) {
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

    pub fn clear_error(&mut self) {
        self.has_error = false;
        self.error_message = None;
    }
}

/// Initialize server state in Tauri app
pub fn init_server_state() -> ServerState {
    ServerState::new()
}

/// Spawn the djinn-server sidecar binary as a detached process
///
/// This uses tauri-plugin-shell to spawn the server as a sidecar.
/// The server runs independently and will survive desktop exit.
///
/// # Arguments
/// * `app` - The Tauri app handle
/// * `timeout_secs` - Maximum time to wait for server to start (default: 30)
///
/// # Returns
/// * `Result<u16, String>` - The port the server is listening on, or an error
pub async fn spawn_server<R: Runtime>(
    app: &AppHandle<R>,
    timeout_secs: u64,
) -> Result<u16, String> {
    // Get the daemon.json path
    let daemon_json_path = get_daemon_json_path(app)?;

    // If daemon.json exists and points to a live, healthy process, reuse it directly.
    if daemon_json_path.exists() {
        if let Some(daemon_info) = read_daemon_info(&daemon_json_path) {
            if is_process_running(daemon_info.pid) {
                if health_check(daemon_info.port).await {
                    log::info!(
                        "Existing healthy server found on port {} (pid {}); skipping spawn",
                        daemon_info.port,
                        daemon_info.pid
                    );
                    let state = app.state::<Mutex<ServerState>>();
                    if let Ok(mut state) = state.lock() {
                        state.port = Some(daemon_info.port);
                        state.mark_healthy();
                    }
                    return Ok(daemon_info.port);
                }
            } else {
                // Stale daemon.json — remove it
                let _ = std::fs::remove_file(&daemon_json_path);
            }
        }
    }

    // Spawn the server sidecar as a detached process
    let sidecar_command = app
        .shell()
        .sidecar("djinn-server")
        .map_err(|e| format!("Failed to create sidecar command: {}", e))?;

    // Spawn the sidecar - it runs independently.
    // If spawn fails (e.g. placeholder binary, permission denied), fall back to
    // discovering an already-running server before giving up.
    let (mut rx, _child) = match sidecar_command.spawn() {
        Ok(result) => result,
        Err(e) => {
            log::warn!("Sidecar spawn failed ({}); checking for existing server", e);
            if let Some(port) = discover_server_for_app(app) {
                if health_check(port).await {
                    log::info!(
                        "Found existing healthy server on port {} after sidecar spawn failure",
                        port
                    );
                    let state = app.state::<Mutex<ServerState>>();
                    if let Ok(mut state) = state.lock() {
                        state.port = Some(port);
                    }
                    return Ok(port);
                }
            }
            return Err(format!("Failed to spawn server sidecar: {}", e));
        }
    };

    // Log any output from the server for debugging
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    eprintln!("[djinn-server stdout] {}", String::from_utf8_lossy(&line));
                }
                CommandEvent::Stderr(line) => {
                    let message = String::from_utf8_lossy(&line);
                    if message
                        .to_ascii_lowercase()
                        .contains("another djinn-server is already running")
                    {
                        eprintln!(
                            "[djinn-server info] Existing daemon already running; reusing it"
                        );
                    } else {
                        eprintln!("[djinn-server stderr] {}", message);
                    }
                }
                CommandEvent::Error(e) => {
                    eprintln!("[djinn-server error] {}", e);
                }
                _ => {}
            }
        }
    });

    // Wait for daemon.json to appear with timeout
    let port = match wait_for_daemon_json(&daemon_json_path, timeout_secs).await {
        Ok(port) => port,
        Err(wait_err) => {
            // If another instance is already running, lock acquisition can fail.
            // Fall back to daemon discovery instead of treating this as fatal.
            if let Some(port) = discover_server_for_app(app) {
                if health_check(port).await {
                    log::info!(
                        "Detected existing server on port {} after spawn attempt; reusing it",
                        port
                    );
                    port
                } else {
                    return Err(wait_err);
                }
            } else {
                return Err(wait_err);
            }
        }
    };

    // Update the server state
    let state = app.state::<Mutex<ServerState>>();
    if let Ok(mut state) = state.lock() {
        state.port = Some(port);
        state.mark_healthy();
    }

    Ok(port)
}

/// Wait for daemon.json to appear, reading the port from it
///
/// Polls every 100ms until the file appears or timeout is reached
async fn wait_for_daemon_json(path: &PathBuf, timeout_secs: u64) -> Result<u16, String> {
    let interval = Duration::from_millis(100);
    let timeout = Duration::from_secs(timeout_secs);
    let start = std::time::Instant::now();

    while start.elapsed() < timeout {
        if path.exists() {
            // Give the server a moment to finish writing the file
            tokio::time::sleep(Duration::from_millis(50)).await;

            match std::fs::read_to_string(path) {
                Ok(content) => {
                    // Parse daemon.json to extract port
                    match serde_json::from_str::<DaemonInfo>(&content) {
                        Ok(daemon_info) => {
                            if daemon_info.port > 0 {
                                return Ok(daemon_info.port);
                            }
                            return Err(format!(
                                "Invalid port in daemon.json: {}",
                                daemon_info.port
                            ));
                        }
                        Err(e) => {
                            // Try parsing as simple JSON with port field
                            match serde_json::from_str::<serde_json::Value>(&content) {
                                Ok(json) => {
                                    if let Some(port) = json.get("port").and_then(|p| p.as_u64()) {
                                        if port > 0 && port <= 65535 {
                                            return Ok(port as u16);
                                        }
                                    }
                                    return Err(format!(
                                        "Invalid port in daemon.json: {}",
                                        content
                                    ));
                                }
                                Err(_) => {
                                    return Err(format!("Failed to parse daemon.json: {}", e));
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    // File might not be fully written yet, continue polling
                    if e.kind() != std::io::ErrorKind::NotFound {
                        eprintln!("Error reading daemon.json: {}, retrying...", e);
                    }
                }
            }
        }

        tokio::time::sleep(interval).await;
    }

    Err(format!(
        "Timeout waiting for daemon.json after {} seconds",
        timeout_secs
    ))
}

/// Get the path to daemon.json/lockfile
///
/// The server writes this file when it is ready and includes the port
fn get_daemon_json_path<R: Runtime>(_app: &AppHandle<R>) -> Result<PathBuf, String> {
    let home_dir =
        dirs::home_dir().ok_or_else(|| "Failed to resolve home directory".to_string())?;
    let daemon_dir = home_dir.join(".djinn");

    // Ensure the directory exists
    std::fs::create_dir_all(&daemon_dir)
        .map_err(|e| format!("Failed to create daemon dir: {}", e))?;

    Ok(daemon_dir.join("daemon.json"))
}

/// Discover an existing server using Tauri's app data directory
pub fn discover_server_for_app<R: Runtime>(app: &AppHandle<R>) -> Option<u16> {
    let daemon_json_path = get_daemon_json_path(app).ok()?;
    discover_server_from_path(&daemon_json_path)
}

fn discover_server_from_path(daemon_json_path: &PathBuf) -> Option<u16> {
    if !daemon_json_path.exists() {
        return None;
    }

    if let Some(daemon_info) = read_daemon_info(daemon_json_path) {
        if is_process_running(daemon_info.pid) {
            Some(daemon_info.port)
        } else {
            let _ = std::fs::remove_file(daemon_json_path);
            None
        }
    } else {
        None
    }
}

fn read_daemon_info(path: &PathBuf) -> Option<DaemonInfo> {
    let content = std::fs::read_to_string(path).ok()?;

    if let Ok(daemon_info) = serde_json::from_str::<DaemonInfo>(&content) {
        return Some(daemon_info);
    }

    let json = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let port = json.get("port").and_then(|p| p.as_u64())?;
    let pid = json.get("pid").and_then(|p| p.as_u64())?;

    if !(1..=65535).contains(&port) {
        return None;
    }

    Some(DaemonInfo {
        port: port as u16,
        pid: pid as u32,
    })
}

/// Check if a process is running by PID
#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION};

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        if handle == 0 || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

/// Check if the server is healthy by polling the health endpoint
pub async fn health_check(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port);

    match reqwest::get(&url).await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

/// Retry server discovery
///
/// This is called from the frontend when user clicks retry button
pub async fn retry_server_discovery<R: Runtime>(app: &AppHandle<R>) -> Result<u16, String> {
    // Clear any existing error state
    if let Some(state) = app.try_state::<Mutex<ServerState>>() {
        if let Ok(mut state) = state.lock() {
            state.clear_error();
        }
    }

    // Try to discover an existing server
    if let Some(port) = discover_server_for_app(app) {
        log::info!("Retry: Discovered existing server on port {}", port);

        // Verify it's healthy
        if health_check(port).await {
            log::info!("Retry: Server on port {} is healthy", port);

            // Update state
            if let Some(state) = app.try_state::<Mutex<ServerState>>() {
                if let Ok(mut state) = state.lock() {
                    state.port = Some(port);
                    state.mark_healthy();
                }
            }

            return Ok(port);
        } else {
            log::warn!("Retry: Discovered server on port {} is not healthy", port);
        }
    }

    // Try to spawn a new server
    match spawn_server(app, 30).await {
        Ok(port) => {
            // Update state
            if let Some(state) = app.try_state::<Mutex<ServerState>>() {
                if let Ok(mut state) = state.lock() {
                    state.port = Some(port);
                    state.mark_healthy();
                }
            }

            Ok(port)
        }
        Err(e) => {
            log::error!("Retry: Failed to spawn server: {}", e);

            // Update state with error
            if let Some(state) = app.try_state::<Mutex<ServerState>>() {
                if let Ok(mut state) = state.lock() {
                    state.mark_error(&e);
                }
            }

            Err(e)
        }
    }
}

/// Start a background health monitor that periodically checks server health.
///
/// When the server becomes unreachable (e.g. after a restart), this monitor
/// re-reads `daemon.json` to discover the new port/PID and updates `ServerState`.
/// Emits Tauri events so the frontend can reconnect SSE and MCP clients.
pub fn start_health_monitor<R: Runtime>(app: &AppHandle<R>) {
    let app_handle = app.clone();

    tauri::async_runtime::spawn(async move {
        // Wait for initial startup to complete before monitoring
        tokio::time::sleep(Duration::from_secs(5)).await;

        let mut was_healthy = true;

        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;

            let current_port = {
                let state = app_handle.state::<Mutex<ServerState>>();
                state.lock().ok().and_then(|s| s.port)
            };

            // If we don't have a port yet, skip (startup hasn't finished)
            let Some(port) = current_port else {
                continue;
            };

            if health_check(port).await {
                if !was_healthy {
                    log::info!("Health monitor: server recovered on port {}", port);
                    if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                        if let Ok(mut state) = state.lock() {
                            state.mark_healthy();
                        }
                    }
                    let _ = app_handle.emit("server:reconnected", port);
                    was_healthy = true;
                }
                continue;
            }

            // Health check failed — try to rediscover from daemon.json
            log::warn!(
                "Health monitor: server on port {} is unreachable, attempting rediscovery",
                port
            );

            if let Some(new_port) = discover_server_for_app(&app_handle) {
                if health_check(new_port).await {
                    log::info!(
                        "Health monitor: rediscovered healthy server on port {} (was {})",
                        new_port,
                        port
                    );
                    if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                        if let Ok(mut state) = state.lock() {
                            state.port = Some(new_port);
                            state.mark_healthy();
                        }
                    }
                    let _ = app_handle.emit("server:reconnected", new_port);
                    was_healthy = true;
                    continue;
                }
            }

            // Server is truly down
            if was_healthy {
                log::warn!("Health monitor: server is down");
                if let Some(state) = app_handle.try_state::<Mutex<ServerState>>() {
                    if let Ok(mut state) = state.lock() {
                        state.is_healthy = false;
                    }
                }
                let _ = app_handle.emit("server:disconnected", ());
                was_healthy = false;
            }
        }
    });
}

// Note: We intentionally do NOT implement a shutdown_server function
// because the server is an independent daemon that should survive
// the desktop application closing. The server manages its own lifecycle.
