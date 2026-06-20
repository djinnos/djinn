use serde::{Deserialize, Serialize};

/// Specification for a named shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    pub command: String,
    pub timeout_secs: Option<u64>,
}

/// Result of running a named shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub name: String,
    /// The shell command that was executed (e.g. `cargo clippy --workspace -- -D warnings`).
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}
