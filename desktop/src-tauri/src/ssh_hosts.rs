//! Persisted SSH host configuration for remote daemon connections.
//!
//! Hosts are stored in `~/.djinn/ssh_hosts.json` and referenced by UUID from
//! the `ConnectionMode::Ssh { host_id }` variant.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshHost {
    /// Unique identifier (UUID v4).
    pub id: String,
    /// Human-readable label, e.g. "My VPS".
    pub label: String,
    /// SSH hostname or IP address.
    pub hostname: String,
    /// SSH username.
    pub user: String,
    /// SSH port (default 22).
    pub port: u16,
    /// Path to SSH private key. `None` means use ssh-agent.
    pub key_path: Option<String>,
    /// Port the remote djinn-server listens on (default 8372).
    pub remote_daemon_port: u16,
    /// Whether djinn-server has been deployed to this host.
    pub deployed: bool,
    /// Version string of the deployed djinn-server, if known.
    pub server_version: Option<String>,
}

impl SshHost {
    /// Create a new host with a fresh UUID and sensible defaults.
    #[allow(dead_code)]
    pub fn new(label: String, hostname: String, user: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            label,
            hostname,
            user,
            port: 22,
            key_path: None,
            remote_daemon_port: 8372,
            deployed: false,
            server_version: None,
        }
    }
}

fn hosts_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".djinn")
        .join("ssh_hosts.json")
}

/// Load all saved SSH hosts from disk.
pub fn load_hosts() -> Vec<SshHost> {
    let path = hosts_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Persist the full host list to disk.
pub fn save_hosts(hosts: &[SshHost]) -> Result<(), String> {
    let path = hosts_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(hosts).map_err(|e| e.to_string())?;
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

/// Find a host by its ID.
pub fn find_host(id: &str) -> Option<SshHost> {
    load_hosts().into_iter().find(|h| h.id == id)
}

/// Add a new host (or replace one with the same ID) and persist.
pub fn add_host(host: SshHost) -> Result<(), String> {
    let mut hosts = load_hosts();
    if let Some(pos) = hosts.iter().position(|h| h.id == host.id) {
        hosts[pos] = host;
    } else {
        hosts.push(host);
    }
    save_hosts(&hosts)
}

/// Remove a host by ID and persist. Returns `Err` if not found.
pub fn remove_host(id: &str) -> Result<(), String> {
    let mut hosts = load_hosts();
    let len_before = hosts.len();
    hosts.retain(|h| h.id != id);
    if hosts.len() == len_before {
        return Err(format!("SSH host '{id}' not found"));
    }
    save_hosts(&hosts)
}

/// Update an existing host in place and persist.
pub fn update_host(host: SshHost) -> Result<(), String> {
    let mut hosts = load_hosts();
    let Some(existing) = hosts.iter_mut().find(|h| h.id == host.id) else {
        return Err(format!("SSH host '{}' not found", host.id));
    };
    *existing = host;
    save_hosts(&hosts)
}
