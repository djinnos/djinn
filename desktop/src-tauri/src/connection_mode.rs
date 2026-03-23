use serde::{Deserialize, Serialize};

/// How the desktop app connects to the djinn server.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionMode {
    /// Spawn and manage a local djinn-server daemon (default).
    ///
    /// The daemon is a separate process that survives desktop restarts and can
    /// be shared with Claude Code and other MCP clients.
    #[default]
    #[serde(alias = "embedded")]
    Daemon,
    /// Connect to an externally-managed server (VPS, WSL, etc.).
    Remote { url: String },
}

/// Load the persisted connection mode, falling back to `Daemon` on any error.
pub fn load() -> ConnectionMode {
    let path = prefs_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return ConnectionMode::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Persist the connection mode to disk.
pub fn save(mode: &ConnectionMode) -> Result<(), String> {
    let path = prefs_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(mode).map_err(|e| e.to_string())?;
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

fn prefs_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".djinn")
        .join("connection_mode.json")
}
