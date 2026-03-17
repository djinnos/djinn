use std::future::Future;
use std::path::Path;
use std::pin::Pin;

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
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

/// Abstracts shell command execution so repository code does not need to
/// import server-level orchestration types.
///
/// The server provides a concrete implementation; verification helpers in
/// `TaskRepository` accept `&dyn VerificationRunner`.
pub trait VerificationRunner: Send + Sync {
    fn run_commands<'a>(
        &'a self,
        specs: &'a [CommandSpec],
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CommandResult>, String>> + Send + 'a>>;
}
