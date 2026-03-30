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
    // We also lower CPU and I/O priority so spawned verification / session
    // commands do not starve interactive user applications (browser, editor).
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::setpgid(0, 0);
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }

            // Nice level 10 — well below default 0, but not starved.
            // Errors are non-fatal: some containers restrict setpriority.
            let _ = libc::setpriority(libc::PRIO_PROCESS as u32, 0, 10);

            // I/O priority: best-effort class (2) with lowest priority (7).
            // ioprio_set is not in libc, use raw syscall.
            #[cfg(target_os = "linux")]
            {
                const IOPRIO_WHO_PROCESS: i32 = 1;
                const IOPRIO_CLASS_BE: i32 = 2;
                // Encoding: (class << 13) | level
                let ioprio_val = (IOPRIO_CLASS_BE << 13) | 7;
                let _ = libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio_val);
            }

            Ok(())
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

/// Join a drain thread with a wall-clock deadline.
///
/// If the thread doesn't finish in time (e.g. a surviving subprocess still
/// holds the pipe open), we abandon it and return whatever bytes it collected
/// up to that point — or an empty vec if it never finished.
#[cfg(unix)]
fn join_with_timeout(
    handle: std::thread::JoinHandle<Vec<u8>>,
    deadline: std::time::Duration,
) -> Vec<u8> {
    // The thread sends its buffer over a channel when done so we can race it
    // against a sleep on the calling thread without unsafe shenanigans.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let buf = handle.join().unwrap_or_default();
        let _ = tx.send(buf);
    });
    rx.recv_timeout(deadline).unwrap_or_default()
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

        // Drain stdout and stderr in background threads to prevent pipe buffer
        // deadlock. The Linux pipe buffer is 64KB — if the child writes more
        // than that before we read, it blocks on write() and wait_timeout()
        // never returns (classic pipe deadlock).
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut out) = stdout {
                let _ = io::Read::read_to_end(&mut out, &mut buf);
            }
            buf
        });
        let stderr_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut err) = stderr {
                let _ = io::Read::read_to_end(&mut err, &mut buf);
            }
            buf
        });

        let timed_out = match child.wait_timeout(timeout)? {
            Some(_status) => false,
            None => {
                let _ = signal_process_group(pgid, libc::SIGTERM);
                std::thread::sleep(std::time::Duration::from_millis(200));

                if child.try_wait()?.is_none() {
                    let _ = signal_process_group(pgid, libc::SIGKILL);
                }

                true
            }
        };
        let status = child.wait()?;

        // After a kill the drain threads may block forever if any subprocess
        // survived in a different process group and still holds the pipe open.
        // Give them a short deadline; take whatever bytes arrived before it.
        let drain_deadline = std::time::Duration::from_secs(if timed_out { 2 } else { 60 });
        let stdout_bytes = join_with_timeout(stdout_handle, drain_deadline);
        let stderr_bytes = join_with_timeout(stderr_handle, drain_deadline);

        Ok(Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        })
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
