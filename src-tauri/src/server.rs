use std::sync::Mutex;
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

/// Spawn the djinn-server sidecar binary
/// 
/// This uses tauri-plugin-shell to spawn the server as a sidecar.
/// The server binary should be named `djinn-server-{target-triple}`
/// and placed in `src-tauri/binaries/`.
pub async fn spawn_server<R: Runtime>(
    _app: &AppHandle<R>,
    _port: u16,
) -> Result<(), String> {
    // TODO: Implement sidecar spawning using tauri-plugin-shell
    // This will spawn the djinn-server binary and monitor its health
    Ok(())
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
