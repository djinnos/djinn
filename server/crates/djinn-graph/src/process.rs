//! Async process spawning via `std::process::Command` + `spawn_blocking`.
//!
//! All subprocess creation in the daemon MUST go through this module rather than
//! using `tokio::process::Command` directly.  The tokio process driver registers
//! child PIDs with the async reactor (kqueue on macOS), and the reactor fd can
//! become stale when the server runs as a background daemon with null stdio,
//! causing every subsequent spawn to fail with EBADF (os error 9).
//!
//! `std::process::Command` avoids this by not touching the reactor at all.
//!
//! ## Cancel-safety / leak avoidance (D5)
//!
//! A naive `spawn_blocking(move || cmd.output())` is *not* cancel-safe: the
//! outer future can be dropped (timeout, task cancel, caller `select!`s away),
//! but the inner blocking `cmd.output()` keeps running on the blocking pool with
//! no way to abort it.  A wedged SCIP indexer or a hung `git fetch` then leaks
//! BOTH the child process AND a blocking-pool thread forever.
//!
//! [`output_with_timeout`] (and the underlying [`output_with_kill`]) fix this by
//! mirroring djinn-agent's command runner: the child is placed in its own
//! process group, and on timeout we signal the whole group SIGTERM, wait a short
//! grace period, then SIGKILL.  stdout/stderr are drained on background threads
//! so the child can't deadlock on a full pipe buffer.  The timeout/kill happens
//! *inside* the blocking closure, so the blocking thread always returns instead
//! of leaking.

use std::io;
use std::process::{Command, Output};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use wait_timeout::ChildExt;

/// Run a pre-configured `std::process::Command` on a blocking thread and return
/// its output.  This is a drop-in async replacement for
/// `tokio::process::Command::output()`.
pub async fn output(mut cmd: Command) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .map_err(io::Error::other)?
}

/// Like [`output`], but aborts if the command does not finish within `timeout`.
/// Returns `io::ErrorKind::TimedOut` **only** when the deadline actually expired.
///
/// Unlike the old implementation (which wrapped [`output`] in
/// `tokio::time::timeout` and leaked the still-running child + blocking thread
/// on expiry), this delegates to [`output_with_kill`], which performs the
/// timeout *inside* the blocking closure and actually reaps the child's process
/// group before returning.
///
/// The `TimedOut` classification is driven by the in-closure "did we fire the
/// deadline kill?" flag, NOT by inspecting the child's exit signal. Re-deriving
/// it from `SIGTERM`/`SIGKILL` conflated our own timeout kill with an EXTERNAL
/// signal — most dangerously the kernel OOM killer's `SIGKILL`, which can reap
/// the child *before* the deadline. Reporting an OOM as `TimedOut` makes the
/// `timed_out` status untrustworthy for callers that treat "the tool hung"
/// differently from "the tool died". A child killed by a signal we did not send
/// (before the deadline) instead surfaces as a distinct non-timeout failure.
pub async fn output_with_timeout(cmd: Command, timeout: Duration) -> io::Result<Output> {
    #[cfg(unix)]
    {
        let (out, deadline_fired) = output_with_kill_inner(cmd, timeout).await?;
        if deadline_fired {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "process timed out"));
        }
        // Deadline did NOT fire, so we never signalled the child ourselves. Any
        // signal death here is external (e.g. OOM killer) — surface it as a
        // distinct failure so it is never miscounted as a timeout.
        if let Some(sig) = killed_by_signal(&out.status) {
            return Err(io::Error::other(format!(
                "process killed by signal {sig} before its deadline"
            )));
        }
        Ok(out)
    }
    #[cfg(not(unix))]
    {
        output_with_kill(cmd, timeout).await
    }
}

/// Run a pre-configured `std::process::Command` on a blocking thread and return
/// its exit status.  This is a drop-in async replacement for
/// `tokio::process::Command::status()`.
pub async fn status(mut cmd: Command) -> io::Result<std::process::ExitStatus> {
    tokio::task::spawn_blocking(move || cmd.status())
        .await
        .map_err(io::Error::other)?
}

/// The signal number that killed the process, if it was terminated by a signal
/// rather than exiting normally. Used to distinguish an externally-signalled
/// death (e.g. the OOM killer) from a clean exit; the caller separately tracks
/// whether *our* deadline kill fired.
#[cfg(unix)]
fn killed_by_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Resolve the nice level applied to subprocesses spawned through the
/// timeout/kill path (in practice: the SCIP indexer fan-out).
///
/// `explicit` is the parsed `DJINN_INDEXER_NICE` override, if any operator
/// set one; `pod_workspace` is whether we're running inside a dedicated warm
/// Pod (signalled by `DJINN_PROJECT_ROOT`, the same signal `index_tree.rs`
/// keys off).
///
/// - **In-process warmer** (djinn-server on a dev box, `pod_workspace = false`):
///   the indexers share the server's cgroup and fan out into `cargo check` /
///   `cc`, so a nice of 10 keeps them from starving interactive request
///   handling. This is the context the original nice was written for.
/// - **Dedicated warm Pod** (`pod_workspace = true`): the indexer is the ONLY
///   workload in the Pod's cgroup — there is nothing intra-pod to yield to, so
///   the nice buys nothing, and with kernel autogrouping edge cases it can only
///   cost the throughput graph freshness depends on. Run at normal priority.
///
/// An explicit `DJINN_INDEXER_NICE` always wins, for hand-tuning.
#[cfg(unix)]
fn resolve_nice_level(explicit: Option<&str>, pod_workspace: bool) -> i32 {
    if let Some(raw) = explicit
        && let Ok(n) = raw.trim().parse::<i32>()
    {
        return n;
    }
    if pod_workspace { 0 } else { 10 }
}

/// Env-reading wrapper around [`resolve_nice_level`]. Called in the PARENT
/// (before fork) so the `pre_exec` closure — which must stay
/// async-signal-safe — never reads the environment itself.
#[cfg(unix)]
fn indexer_nice_level() -> i32 {
    let explicit = std::env::var("DJINN_INDEXER_NICE").ok();
    let pod_workspace = std::env::var("DJINN_PROJECT_ROOT").is_ok();
    resolve_nice_level(explicit.as_deref(), pod_workspace)
}

#[cfg(unix)]
fn isolate_process_group(cmd: &mut Command) {
    // Resolve the nice level HERE, in the parent thread before fork, so the
    // pre_exec closure below stays async-signal-safe (no env reads after fork).
    let nice = indexer_nice_level();
    // SAFETY: pre_exec runs in the child process right before exec.
    // setpgid(0, 0) places that child in a new process group so the whole
    // group (the indexer/git child plus anything it forks) can be signalled
    // with a single `kill(-pgid, …)`.
    unsafe {
        cmd.pre_exec(move || {
            let rc = libc::setpgid(0, 0);
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }

            // Best-effort renice. `nice == 0` is normal priority (the kernel
            // default), so skip the syscall entirely there — in a dedicated
            // warm Pod the indexer owns the whole cgroup and must not yield.
            // In-process (dev box) `nice == 10` keeps the indexer fan-out from
            // starving interactive server request handling.
            if nice != 0 {
                let _ = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
            }

            Ok(())
        });
    }
}

/// Join a drain thread with a wall-clock deadline.
///
/// If the thread doesn't finish in time (e.g. a surviving subprocess still
/// holds the pipe open), we abandon it and return whatever bytes it collected
/// up to that point — or an empty vec if it never finished.
#[cfg(unix)]
fn join_with_timeout(handle: std::thread::JoinHandle<Vec<u8>>, deadline: Duration) -> Vec<u8> {
    // The thread sends its buffer over a channel when done so we can race it
    // against a recv timeout without unsafe shenanigans.
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

/// Run `cmd` on a blocking thread, killing its entire process group if it does
/// not finish within `timeout`.
///
/// The kill escalates SIGTERM → (200ms grace) → SIGKILL, and stdout/stderr are
/// drained on background threads to avoid the classic full-pipe deadlock (the
/// Linux pipe buffer is 64KiB; a child that writes more before we read blocks on
/// `write()` and `wait_timeout` would never return).
///
/// Because the timeout and kill happen *inside* the blocking closure, the
/// blocking-pool thread always returns rather than leaking when the caller's
/// future is dropped.
#[cfg(unix)]
pub async fn output_with_kill(cmd: Command, timeout: Duration) -> io::Result<Output> {
    output_with_kill_inner(cmd, timeout)
        .await
        .map(|(out, _timed_out)| out)
}

/// Inner form of [`output_with_kill`] that also reports whether *our* deadline
/// kill fired. `true` means the timeout expired and we signalled the child's
/// process group; `false` means the child exited (cleanly or via an external
/// signal) on its own. [`output_with_timeout`] relies on this flag rather than
/// on the child's exit signal so an OOM-killer `SIGKILL` before the deadline is
/// not misreported as a timeout.
#[cfg(unix)]
async fn output_with_kill_inner(mut cmd: Command, timeout: Duration) -> io::Result<(Output, bool)> {
    // The caller's `Command` may not have piped stdio (e.g. SCIP indexer plans
    // build a bare `Command`).  We take the pipes ourselves, so force them.
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    isolate_process_group(&mut cmd);

    tokio::task::spawn_blocking(move || {
        let mut child = cmd.spawn()?;
        let pgid = child.id() as i32;

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
                std::thread::sleep(Duration::from_millis(200));

                if child.try_wait()?.is_none() {
                    let _ = signal_process_group(pgid, libc::SIGKILL);
                }

                true
            }
        };
        // Reap the child so it doesn't linger as a zombie. After SIGKILL this
        // returns promptly.
        let status = child.wait()?;

        // After a kill the drain threads may block if a subprocess survived in a
        // different process group and still holds the pipe open. Give them a
        // short deadline; take whatever bytes arrived before it.
        let drain_deadline = Duration::from_secs(if timed_out { 2 } else { 60 });
        let stdout_bytes = join_with_timeout(stdout_handle, drain_deadline);
        let stderr_bytes = join_with_timeout(stderr_handle, drain_deadline);

        Ok((
            Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            },
            timed_out,
        ))
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(not(unix))]
pub async fn output_with_kill(mut cmd: Command, _timeout: Duration) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .map_err(io::Error::other)?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use djinn_core::clock::{Clock, SystemClock};

    /// A hung child (`sleep 30`) must be terminated within the grace window and
    /// the call must return promptly rather than hanging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_kills_hung_child_promptly() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30");

        let start = SystemClock::new().now_instant();
        let err = output_with_timeout(cmd, Duration::from_millis(200))
            .await
            .expect_err("hung child should surface as a timeout error");
        let elapsed = start.elapsed();

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        // 200ms timeout + 200ms SIGTERM grace + drain deadline; comfortably
        // under a second. If the kill path leaked, this would block ~30s.
        assert!(
            elapsed < Duration::from_secs(5),
            "call should return promptly after kill, took {elapsed:?}"
        );
    }

    /// The child runs in its own process group, and after the timeout kill the
    /// whole group is gone (no leaked grandchildren).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_reaps_whole_process_group() {
        // The shell prints its pgid, then forks a grandchild that sleeps. If the
        // group kill works, signalling -pgid reaches the grandchild too.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("ps -o pgid= -p $$ | tr -d ' '; sleep 30 & sleep 30");

        let out = output_with_kill(cmd, Duration::from_millis(200))
            .await
            .expect("process should be reaped after timeout kill");

        // Killed by signal => not a clean success.
        assert!(!out.status.success());
        assert!(killed_by_signal(&out.status).is_some());

        let pgid: i32 = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .parse()
            .expect("child should have printed its pgid");

        // Give the kernel a beat to tear the group down, then confirm no member
        // of the group is still alive. kill(-pgid, 0) returning ESRCH means the
        // group is empty.
        std::thread::sleep(Duration::from_millis(300));
        let rc = unsafe { libc::kill(-pgid, 0) };
        if rc == 0 {
            panic!("process group {pgid} survived the timeout kill — leaked");
        }
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "expected the killed process group to be fully gone"
        );
    }

    /// A child killed by an EXTERNAL signal (modelling the kernel OOM killer)
    /// *before* the deadline must NOT be reported as a timeout — otherwise the
    /// `timed_out` status becomes untrustworthy. It surfaces as a distinct
    /// non-timeout failure instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_signal_before_deadline_is_not_a_timeout() {
        // The shell SIGKILLs itself immediately, well within the generous
        // 30s deadline, so our timeout-kill path never fires.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("kill -9 $$");

        let err = output_with_timeout(cmd, Duration::from_secs(30))
            .await
            .expect_err("a signal-killed child must surface as an error");

        assert_ne!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "an external SIGKILL before the deadline must not be reported as a timeout"
        );
        assert!(
            err.to_string().contains("signal"),
            "expected a distinct signal-death message, got: {err}"
        );
    }

    /// The in-process warmer (no `DJINN_PROJECT_ROOT`) keeps nice 10 so the
    /// indexer fan-out can't starve the interactive server it shares a cgroup
    /// with.
    #[test]
    fn nice_level_defaults_to_10_in_process() {
        assert_eq!(resolve_nice_level(None, false), 10);
    }

    /// In a dedicated warm Pod the indexer owns the whole cgroup — nothing to
    /// yield to — so it must run at normal priority (0), not niced. This is the
    /// core of the starvation fix: niceing a sole-tenant cgroup only costs the
    /// throughput graph freshness depends on.
    #[test]
    fn nice_level_is_zero_in_pod_workspace() {
        assert_eq!(resolve_nice_level(None, true), 0);
    }

    /// An explicit `DJINN_INDEXER_NICE` override wins in either context.
    #[test]
    fn explicit_nice_override_wins() {
        assert_eq!(resolve_nice_level(Some("5"), true), 5);
        assert_eq!(resolve_nice_level(Some("15"), false), 15);
        assert_eq!(resolve_nice_level(Some(" 3 "), true), 3);
    }

    /// A malformed override falls back to the context default rather than
    /// crashing the spawn path.
    #[test]
    fn malformed_nice_override_falls_back() {
        assert_eq!(resolve_nice_level(Some("not-a-number"), true), 0);
        assert_eq!(resolve_nice_level(Some(""), false), 10);
    }

    /// A fast command still returns its real output and a success status.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fast_command_returns_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf hello");

        let out = output_with_timeout(cmd, Duration::from_secs(10))
            .await
            .expect("fast command succeeds");
        assert!(out.status.success());
        assert_eq!(&out.stdout, b"hello");
    }
}
