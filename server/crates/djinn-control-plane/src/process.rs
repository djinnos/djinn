//! Async process spawning via `std::process::Command` + `spawn_blocking`.
//!
//! All subprocess creation in the daemon MUST go through this module rather than
//! using `tokio::process::Command` directly.  The tokio process driver registers
//! child PIDs with the async reactor (kqueue on macOS), and the reactor fd can
//! become stale when the server runs as a background daemon with null stdio,
//! causing every subsequent spawn to fail with EBADF (os error 9).
//!
//! `std::process::Command` avoids this by not touching the reactor at all.

use std::io;
use std::process::{Command, Output};

/// Run a pre-configured `std::process::Command` on a blocking thread and return
/// its output.  This is a drop-in async replacement for
/// `tokio::process::Command::output()`.
pub async fn output(mut cmd: Command) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .map_err(io::Error::other)?
}

/// Run a pre-configured `std::process::Command` on a blocking thread and return
/// its exit status.  This is a drop-in async replacement for
/// `tokio::process::Command::status()`.
pub async fn status(mut cmd: Command) -> io::Result<std::process::ExitStatus> {
    tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(io::Error::other)?
}
