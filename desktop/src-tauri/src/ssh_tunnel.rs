//! SSH tunnel lifecycle management.
//!
//! Spawns `ssh -N -L ...` as a child process and keeps a module-level global
//! so the tunnel can be stopped on app exit (the `Child` handle is not
//! `Send`-safe enough for Tauri managed state).

use crate::ssh_hosts::SshHost;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::net::TcpListener;
use std::process::{Child, Command};
use std::sync::Mutex;

/// Active SSH tunnel handle.
pub struct SshTunnel {
    child: Child,
    pub local_port: u16,
    pub _remote_port: u16,
    pub host_id: String,
}

/// Observable tunnel status emitted to the frontend.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TunnelStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected { local_port: u16 },
    Reconnecting,
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Global tunnel state
// ---------------------------------------------------------------------------

static ACTIVE_TUNNEL: Lazy<Mutex<Option<SshTunnel>>> = Lazy::new(|| Mutex::new(None));

/// Store a newly-created tunnel as the active one, stopping any previous tunnel.
pub fn set_active_tunnel(tunnel: SshTunnel) {
    let mut guard = ACTIVE_TUNNEL.lock().unwrap();
    if let Some(mut old) = guard.take() {
        stop_tunnel(&mut old);
    }
    *guard = Some(tunnel);
}

/// Stop and remove the active tunnel (if any).
pub fn stop_active_tunnel() {
    let mut guard = ACTIVE_TUNNEL.lock().unwrap();
    if let Some(mut t) = guard.take() {
        stop_tunnel(&mut t);
    }
}

/// Check whether the active tunnel's SSH process is still alive.
pub fn is_active_tunnel_alive() -> bool {
    let mut guard = ACTIVE_TUNNEL.lock().unwrap();
    match guard.as_mut() {
        Some(t) => is_alive(t),
        None => false,
    }
}

/// Get the local port of the active tunnel (if connected).
#[allow(dead_code)]
pub fn active_tunnel_local_port() -> Option<u16> {
    let guard = ACTIVE_TUNNEL.lock().unwrap();
    guard.as_ref().map(|t| t.local_port)
}

/// Get the host ID of the active tunnel (if any).
pub fn active_tunnel_host_id() -> Option<String> {
    let guard = ACTIVE_TUNNEL.lock().unwrap();
    guard.as_ref().map(|t| t.host_id.clone())
}

// ---------------------------------------------------------------------------
// Tunnel operations
// ---------------------------------------------------------------------------

/// Start an SSH tunnel to the given host.
///
/// Finds a free local port, then spawns:
/// ```text
/// ssh -N -L {local_port}:127.0.0.1:{remote_port} -p {port} [-i key] user@host
/// ```
pub fn start_tunnel(host: &SshHost) -> Result<SshTunnel, String> {
    let local_port = find_free_port()?;

    let mut cmd = Command::new("ssh");
    cmd.arg("-N")
        .arg("-L")
        .arg(format!(
            "{}:127.0.0.1:{}",
            local_port, host.remote_daemon_port
        ))
        .arg("-p")
        .arg(host.port.to_string())
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new");

    if let Some(ref key) = host.key_path {
        cmd.arg("-i").arg(key);
    }

    cmd.arg(format!("{}@{}", host.user, host.hostname));

    // Detach stdin/stdout/stderr so the child doesn't block.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let child = cmd.spawn().map_err(|e| {
        format!(
            "Failed to spawn SSH tunnel to {}@{}:{}: {}",
            host.user, host.hostname, host.port, e
        )
    })?;

    log::info!(
        "SSH tunnel started: local {} -> {}@{}:{}",
        local_port,
        host.user,
        host.hostname,
        host.remote_daemon_port
    );

    Ok(SshTunnel {
        child,
        local_port,
        _remote_port: host.remote_daemon_port,
        host_id: host.id.clone(),
    })
}

/// Kill the SSH child process.
pub fn stop_tunnel(tunnel: &mut SshTunnel) {
    log::info!(
        "Stopping SSH tunnel (local port {}, host {})",
        tunnel.local_port,
        tunnel.host_id
    );
    let _ = tunnel.child.kill();
    let _ = tunnel.child.wait();
}

/// Returns `true` if the SSH child process is still running.
pub fn is_alive(tunnel: &mut SshTunnel) -> bool {
    matches!(tunnel.child.try_wait(), Ok(None))
}

/// Ensure the remote djinn-server daemon is running on the host.
///
/// SSH in and start it if `pgrep` doesn't find it.
pub async fn ensure_remote_daemon(host: &SshHost) -> Result<(), String> {
    let cmd = format!(
        "pgrep -f djinn-server || (nohup ~/.djinn/bin/djinn-server --port {} &>/dev/null &)",
        host.remote_daemon_port
    );
    let output = ssh_exec(host, &cmd)?;
    log::info!("ensure_remote_daemon output: {}", output.trim());
    Ok(())
}

/// Test SSH connectivity to a host.
///
/// Returns the output of `echo ok && uname -a` on success.
pub fn test_connection(host: &SshHost) -> Result<String, String> {
    ssh_exec_with_timeout(host, "echo ok && uname -a", 5)
}

/// Run a command on the remote host via SSH.
pub fn ssh_exec(host: &SshHost, command: &str) -> Result<String, String> {
    ssh_exec_with_timeout(host, command, 10)
}

/// Run a command on the remote host via SSH with a custom timeout.
fn ssh_exec_with_timeout(host: &SshHost, command: &str, timeout: u32) -> Result<String, String> {
    let mut cmd = Command::new("ssh");
    cmd.arg("-p")
        .arg(host.port.to_string())
        .arg("-o")
        .arg(format!("ConnectTimeout={}", timeout))
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new");

    if let Some(ref key) = host.key_path {
        cmd.arg("-i").arg(key);
    }

    cmd.arg(format!("{}@{}", host.user, host.hostname));
    cmd.arg(command);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to execute SSH command: {}", e))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(format!("SSH command failed: {}", stderr.trim()))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find a free TCP port on localhost by binding to port 0.
fn find_free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| {
        format!("Failed to find free port: {}", e)
    })?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to get local addr: {}", e))?
        .port();
    Ok(port)
}
