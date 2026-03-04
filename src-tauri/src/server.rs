use std::path::PathBuf;
use std::time::Duration;
use std::sync::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};
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

    // Remove any existing daemon.json from previous runs
    if daemon_json_path.exists() {
        let _ = std::fs::remove_file(&daemon_json_path);
    }

    // Spawn the server sidecar as a detached process
    let sidecar_command = app
        .shell()
        .sidecar("djinn-server")
        .map_err(|e| format!("Failed to create sidecar command: {}", e))?;

    // Spawn the sidecar - it runs independently
    let (mut rx, _child) = sidecar_command
        .spawn()
        .map_err(|e| format!("Failed to spawn server sidecar: {}", e))?;

    // Log any output from the server for debugging
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    eprintln!("[djinn-server stdout] {}", String::from_utf8_lossy(&line));
                }
                CommandEvent::Stderr(line) => {
                    eprintln!("[djinn-server stderr] {}", String::from_utf8_lossy(&line));
                }
                CommandEvent::Error(e) => {
                    eprintln!("[djinn-server error] {}", e);
                }
                _ => {}
            }
        }
    });

    // Wait for daemon.json to appear with timeout
    let port = wait_for_daemon_json(&daemon_json_path, timeout_secs).await?;

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
async fn wait_for_daemon_json(
    path: &PathBuf,
    timeout_secs: u64,
) -> Result<u16, String> {
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
                            if daemon_info.port > 0 && daemon_info.port <= 65535 {
                                return Ok(daemon_info.port);
                            }
                            return Err(format!("Invalid port in daemon.json: {}", daemon_info.port));
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
                                    return Err(format!("Invalid port in daemon.json: {}", content));
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
fn get_daemon_json_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    // Ensure the directory exists
    std::fs::create_dir_all(&app_data_dir)
        .map_err(|e| format!("Failed to create app data dir: {}", e))?;

    Ok(app_data_dir.join("daemon.json"))
}

/// Discover an existing server by reading daemon.json
///
/// Returns the port if a valid daemon.json exists, None otherwise
pub fn discover_server() -> Option<u16> {
    // Get the app data directory
    let app_data_dir = dirs::data_dir()?.join("com.djinnos.desktop");
    let daemon_json_path = app_data_dir.join("daemon.json");

    if !daemon_json_path.exists() {
        return None;
    }

    match std::fs::read_to_string(&daemon_json_path) {
        Ok(content) => {
            match serde_json::from_str::<DaemonInfo>(&content) {
                Ok(daemon_info) => {
                    // Validate PID is still running
                    if is_process_running(daemon_info.pid) {
                        Some(daemon_info.port)
                    } else {
                        // Process is dead, remove stale daemon.json
                        let _ = std::fs::remove_file(&daemon_json_path);
                        None
                    }
                }
                Err(_) => {
                    // Try simple JSON parsing
                    match serde_json::from_str::<serde_json::Value>(&content) {
                        Ok(json) => {
                            json.get("port").and_then(|p| p.as_u64()).map(|p| p as u16)
                        }
                        Err(_) => None,
                    }
                }
            }
        }
        Err(_) => None,
    }
}

/// Check if a process is running by PID
#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
    }
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

/// Check if server is ready (has started and daemon.json exists)
pub fn is_server_ready<R: Runtime>(app: &AppHandle<R>) -> bool {
    if let Ok(state) = app.state::<Mutex<ServerState>>().lock() {
        state.ready
    } else {
        false
    }
}

/// Get the server port from state
pub fn get_server_port<R: Runtime>(app: &AppHandle<R>) -> Option<u16> {
    if let Ok(state) = app.state::<Mutex<ServerState>>().lock() {
        state.port
    } else {
        None
    }
}

/// Retry server discovery
///
/// This is called from the frontend when user clicks retry button
pub async fn retry_server_discovery<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<u16, String> {
    // Clear any existing error state
    if let Some(state) = app.try_state::<Mutex<ServerState>>() {
        if let Ok(mut state) = state.lock() {
            state.clear_error();
        }
    }

    // Try to discover an existing server
    if let Some(port) = discover_server() {
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

// Note: We intentionally do NOT implement a shutdown_server function
// because the server is an independent daemon that should survive
// the desktop application closing. The server manages its own lifecycle.
