use std::path::PathBuf;
use std::sync::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime};

/// Server state managed by Tauri
pub struct ServerState {
    pub child: Option<std::process::Child>,
    pub port: u16,
}

impl ServerState {
    pub fn new(port: u16) -> Self {
        Self { child: None, port }
    }
}

/// Initialize server state in Tauri app
pub fn init_server_state(port: u16) -> ServerState {
    ServerState::new(port)
}

/// Daemon info from daemon.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: String,
}

/// Path to the daemon.json file
fn daemon_json_path() -> PathBuf {
    let home = dirs::home_dir().expect("Failed to get home directory");
    home.join(".djinn").join("daemon.json")
}

/// Read daemon.json and parse daemon info
pub fn read_daemon_json() -> Result<DaemonInfo, String> {
    let path = daemon_json_path();
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read daemon.json: {}", e))?;
    
    let info: DaemonInfo = serde_json::from_str(&contents)
        .map_err(|e| format!("Failed to parse daemon.json: {}", e))?;
    
    Ok(info)
}

/// Check if a PID is alive (platform-specific)
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    unsafe {
        // kill(pid, 0) returns 0 if process exists and we have permission to signal it
        // returns -1 with errno set if process doesn't exist or we don't have permission
        libc::kill(pid as i32, 0) == 0
    }
}

#[cfg(windows)]
pub fn is_pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == INVALID_HANDLE_VALUE || handle == 0 {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

/// Discover running server by reading daemon.json and validating PID
/// 
/// Returns the port if a valid running server is found, None otherwise
pub fn discover_server() -> Option<u16> {
    match read_daemon_json() {
        Ok(info) => {
            if is_pid_alive(info.pid) {
                Some(info.port)
            } else {
                log::info!("Daemon PID {} is not alive", info.pid);
                None
            }
        }
        Err(e) => {
            log::debug!("Failed to discover server: {}", e);
            None
        }
    }
}

/// Spawn the djinn-server sidecar binary
/// 
/// This uses tauri-plugin-shell to spawn the server as a sidecar.
/// The server binary should be named `djinn-server-{target-triple}`
/// and placed in `src-tauri/binaries/`.
/// 
/// First attempts to discover an existing server via daemon.json,
/// only spawns a new server if none is found.
pub async fn spawn_server<R: Runtime>(
    _app: &AppHandle<R>,
    _port: u16,
) -> Result<u16, String> {
    // First, try to discover an existing server
    if let Some(port) = discover_server() {
        log::info!("Found existing server on port {}", port);
        
        // Verify it's healthy before returning
        if health_check(port).await {
            log::info!("Existing server is healthy, connecting to port {}", port);
            return Ok(port);
        } else {
            log::warn!("Existing server on port {} is not healthy, will spawn new server", port);
        }
    }
    
    // TODO: Implement sidecar spawning using tauri-plugin-shell
    // This will spawn the djinn-server binary and monitor its health
    // For now, return an error since we haven't found a server and haven't implemented spawning
    Err("No running server found and sidecar spawning not yet implemented".to_string())
}

/// Check if the server is healthy by polling the health endpoint
pub async fn health_check(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port);
    
    match reqwest::get(&url).await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

/// Gracefully shutdown the server
pub async fn shutdown_server(state: &Mutex<ServerState>) -> Result<(), String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    
    if let Some(mut child) = state.child.take() {
        // Kill the child process (sends SIGTERM on Unix, TerminateProcess on Windows)
        let _ = child.kill();
        
        // Wait for process to exit
        let _ = child.wait();
    }
    
    Ok(())
}
