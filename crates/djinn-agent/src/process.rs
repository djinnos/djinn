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

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use tokio::time::Duration;

#[cfg(unix)]
use wait_timeout::ChildExt;

#[cfg(unix)]
pub fn isolate_process_group(cmd: &mut Command) {
    // SAFETY: pre_exec runs in the child process right before exec.
    // setpgid(0, 0) places that child in a new process group.
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::setpgid(0, 0);
            if rc == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
pub fn isolate_process_group(_cmd: &mut Command) {}

/// Run a pre-configured `std::process::Command` on a blocking thread and return
/// its output.  This is a drop-in async replacement for
/// `tokio::process::Command::output()`.
pub async fn output(mut cmd: Command) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .map_err(io::Error::other)?
}

#[cfg(unix)]
fn signal_process_group(pgid: i32, signal: libc::c_int) -> io::Result<()> {
    // Negative pid targets the whole process group.
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // Already exited.
            Some(libc::ESRCH) => Ok(()),
            _ => Err(err),
        }
    }
}

#[cfg(unix)]
pub async fn output_with_kill(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || {
        let mut child = cmd.spawn()?;
        let pgid = child.id() as i32;

        match child.wait_timeout(timeout)? {
            Some(_status) => child.wait_with_output(),
            None => {
                let _ = signal_process_group(pgid, libc::SIGTERM);
                std::thread::sleep(std::time::Duration::from_millis(200));

                if child.try_wait()?.is_none() {
                    let _ = signal_process_group(pgid, libc::SIGKILL);
                }

                child.wait_with_output()
            }
        }
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(not(unix))]
pub async fn output_with_kill(cmd: Command, _timeout: Duration) -> io::Result<Output> {
    output(cmd).await
}

/// Run a pre-configured `std::process::Command` on a blocking thread and return
/// its exit status.  This is a drop-in async replacement for
/// `tokio::process::Command::status()`.
pub async fn status(mut cmd: Command) -> io::Result<std::process::ExitStatus> {
    tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(io::Error::other)?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawned_process_uses_different_pgid() {
        let parent_pgid = unsafe { libc::getpgrp() };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf '%d' \"$(ps -o pgid= -p $$)\"");
        isolate_process_group(&mut cmd);

        let output = output(cmd).await.expect("spawn succeeds");
        assert!(output.status.success());

        let child_pgid: i32 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("child pgid is parseable");

        assert_ne!(child_pgid, parent_pgid);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_kills_sleep_process() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 999");
        isolate_process_group(&mut cmd);

        let out = output_with_kill(cmd, Duration::from_millis(100))
            .await
            .expect("process should be reaped after timeout kill");
        assert!(!out.status.success());
    }
}
