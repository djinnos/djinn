//! WSL (Windows Subsystem for Linux) support.
//!
//! On Windows, the desktop app can launch and connect to a djinn-server running
//! inside the default WSL distribution. WSL2 shares localhost networking so the
//! server is reachable at `127.0.0.1:{port}`.
//!
//! On non-Windows platforms all functions are no-ops / return `false`.

/// Check if WSL is available on this machine.
#[cfg(target_os = "windows")]
pub fn is_available() -> bool {
    std::process::Command::new("wsl")
        .arg("--status")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
pub fn is_available() -> bool {
    false
}

/// Ensure a djinn-server daemon is running inside the default WSL distribution.
///
/// Returns the base URL on success (always `http://127.0.0.1:{port}` because
/// WSL2 shares localhost).
#[cfg(target_os = "windows")]
pub async fn ensure_wsl_daemon(port: u16) -> Result<String, String> {
    // Check if already running.
    let check = std::process::Command::new("wsl")
        .args(["-e", "pgrep", "-f", "djinn-server"])
        .output()
        .map_err(|e| format!("Failed to run WSL pgrep: {}", e))?;

    if !check.status.success() {
        // Not running — start it.
        log::info!("Starting djinn-server inside WSL on port {port}");
        let start = std::process::Command::new("wsl")
            .args([
                "-e",
                "sh",
                "-c",
                &format!("nohup djinn-server --port {port} &>/dev/null &"),
            ])
            .output()
            .map_err(|e| format!("Failed to start djinn-server in WSL: {}", e))?;

        if !start.status.success() {
            let stderr = String::from_utf8_lossy(&start.stderr).to_string();
            return Err(format!("WSL djinn-server start failed: {}", stderr.trim()));
        }

        // Give it a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let base_url = format!("http://127.0.0.1:{}", port);

    // Health-check loop.
    for _ in 0..40 {
        if crate::server::health_check(&base_url).await {
            return Ok(base_url);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    Err("WSL djinn-server started but health endpoint not reachable".into())
}

#[cfg(not(target_os = "windows"))]
pub async fn ensure_wsl_daemon(port: u16) -> Result<String, String> {
    Err(format!(
        "WSL is not available on this platform (requested port {port})"
    ))
}
