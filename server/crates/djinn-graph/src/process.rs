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
//!
//! ## Linux: cancellation-independent supervision
//!
//! On Linux the process path is supervised by the worker-wide child reaper
//! ([`crate::child_reaper`]).  Admission is reserved *before* spawn, the direct
//! PID/PGID is registered immediately after spawn, and terminal status arrives
//! only from the central reaper — never from `Child::wait`/`try_wait`.  An owned
//! supervisor retains stdout/stderr drain and cleanup ownership even after the
//! caller future is dropped.  Completion is gated on direct status, original PGID
//! disappearance (`kill(-pgid, 0)` → `ESRCH`), and a reaper quiescence pass.
//! Normal direct exit gets 2 s natural grace before TERM (2 s) and KILL (2 s);
//! timeout or caller cancellation skips natural grace and starts TERM
//! immediately.

use std::io;
use std::process::{Command, Output};
use std::time::Duration;
use std::{error::Error, fmt};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

// ---------------------------------------------------------------------------
// Platform-specific imports
// ---------------------------------------------------------------------------

/// Non-Linux Unix still uses the legacy `wait_timeout`-based inner path.
#[cfg(all(unix, not(target_os = "linux")))]
use wait_timeout::ChildExt;

/// Linux uses the worker-wide child reaper for all direct-child status.
#[cfg(target_os = "linux")]
use crate::child_reaper::{DirectChild, TerminalStatus, worker_child_reaper};

/// `Instant` for deadline/poll timing inside the supervisor.
#[cfg(target_os = "linux")]
use std::time::Instant;

/// Unit-only lifecycle acknowledgement for the Linux supervisor. The child
/// reaper removes a direct-PID route as soon as it receives terminal status,
/// which is intentionally earlier than closing the supervisor-owned pipes.
/// Keep this separate from the production registry so regression fixtures can
/// prove the latter ownership has ended.
#[cfg(all(test, target_os = "linux"))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Clock abstraction for deterministic timing inside the supervisor.
#[cfg(target_os = "linux")]
use djinn_core::clock::{Clock, SystemClock};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The bounded lifecycle phase which was unable to establish cleanup.
///
/// `Term` and `Kill` refer to the respective process-group signal windows;
/// `ReaperQuiescence` means the group disappeared but the worker-wide reaper
/// did not complete its final acknowledgement pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupPhase {
    /// The SIGTERM cleanup window expired before the group disappeared.
    Term,
    /// The SIGKILL cleanup window expired before the group disappeared.
    Kill,
    /// The group was gone but the child reaper did not quiesce.
    ReaperQuiescence,
}

impl fmt::Display for CleanupPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term => f.write_str("SIGTERM"),
            Self::Kill => f.write_str("SIGKILL"),
            Self::ReaperQuiescence => f.write_str("reaper quiescence"),
        }
    }
}

/// Diagnostic returned when the Linux command supervisor exhausts cleanup.
///
/// Process APIs retain their `io::Result` signatures. On this exceptional path
/// the returned [`io::Error`] contains this type as its source, and this error
/// in turn exposes the command's original timeout/failure through
/// [`Error::source`].
#[derive(Debug)]
pub struct CleanupExhaustionError {
    /// Supervisor-generated command identifier.
    pub command_id: String,
    /// Original process group identifier.
    pub pgid: i32,
    /// Terminal status for the registered direct child, if it was reaped.
    pub direct_status: Option<crate::child_reaper::TerminalStatus>,
    /// Last lifecycle phase attempted before cleanup was declared exhausted.
    pub phase: CleanupPhase,
    /// PIDs still present in Linux `/proc` for this process group at exhaustion.
    pub surviving_pids: Vec<u32>,
    source: Box<dyn Error + Send + Sync>,
}

impl CleanupExhaustionError {
    fn new(
        command_id: String,
        pgid: i32,
        direct_status: Option<crate::child_reaper::TerminalStatus>,
        phase: CleanupPhase,
        surviving_pids: Vec<u32>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            command_id,
            pgid,
            direct_status,
            phase,
            surviving_pids,
            source: Box::new(source),
        }
    }
}

impl fmt::Display for CleanupExhaustionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "command {} cleanup exhausted during {} for process group {} (surviving /proc PIDs: {:?})",
            self.command_id, self.phase, self.pgid, self.surviving_pids
        )
    }
}

impl Error for CleanupExhaustionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

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
    #[cfg(target_os = "linux")]
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
    #[cfg(not(target_os = "linux"))]
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

// ---------------------------------------------------------------------------
// Shared Unix helpers
// ---------------------------------------------------------------------------

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
#[cfg(all(unix, not(target_os = "linux")))]
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

/// Reconstruct an `ExitStatus` from a raw wait status word.  Used by the Linux
/// supervisor because the central reaper supplies `TerminalStatus` (raw status),
/// not a `std::process::ExitStatus`.
#[cfg(target_os = "linux")]
fn status_from_raw(raw_status: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(raw_status)
}

// ===========================================================================
// Linux: cancellation-independent command supervisor
// ===========================================================================

/// Run `cmd` on a blocking thread, killing its entire process group if it does
/// not finish within `timeout`.
///
/// On Linux this is supervised by the worker-wide child reaper.  See the module
/// docs for the full lifecycle contract.
#[cfg(target_os = "linux")]
pub async fn output_with_kill(cmd: Command, timeout: Duration) -> io::Result<Output> {
    output_with_kill_inner(cmd, timeout)
        .await
        .map(|(out, _timed_out)| out)
}

/// Inner form of [`output_with_kill`] that also reports whether *our* deadline
/// kill fired.
///
/// The supervisor owns:
/// - admission reservation (before spawn)
/// - direct PID/PGID registration (immediately after spawn)
/// - stdout/stderr drain threads
/// - direct-status receipt from the central reaper
/// - PGID signaling and disappearance polling
/// - reaper quiescence pass
///
/// A cancellation channel detects caller-future drop: the caller owns the
/// `Sender` side; dropping it disconnects the channel, which the supervisor
/// observes and converts into immediate TERM/KILL cleanup (skipping natural
/// grace).
#[cfg(target_os = "linux")]
async fn output_with_kill_inner(mut cmd: Command, timeout: Duration) -> io::Result<(Output, bool)> {
    // The caller's `Command` may not have piped stdio (e.g. SCIP indexer plans
    // build a bare `Command`).  We take the pipes ourselves, so force them.
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    isolate_process_group(&mut cmd);

    // Cancellation channel: the Sender lives in this async frame.  If the caller
    // drops the future (timeout at a higher level, task cancel, select! away),
    // the Sender is dropped, the channel disconnects, and the supervisor
    // detects it and starts bounded cleanup immediately.
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();

    let handle = tokio::task::spawn_blocking(move || run_linux_supervisor(cmd, timeout, cancel_rx));

    let result = handle.await.map_err(io::Error::other)?;
    // If we reach here the future was NOT cancelled — drop the sender cleanly.
    // (It would be dropped anyway when the frame ends, but being explicit makes
    // the lifecycle obvious.)
    drop(cancel_tx);
    result
}

/// Polling interval for PGID disappearance and direct-status checks (~25 ms).
#[cfg(target_os = "linux")]
const PGID_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Duration of each cleanup phase (natural grace, TERM, KILL): 2 seconds each.
#[cfg(target_os = "linux")]
const PHASE_TIMEOUT: Duration = Duration::from_secs(2);

/// Number of Linux supervisors which have not returned from
/// [`run_linux_supervisor`]. This is deliberately test-only: it acknowledges
/// completion after `LinuxDrains::finish` has synchronously closed pipe FDs,
/// unlike the child-reaper registry whose direct route is removed at status
/// delivery time.
#[cfg(all(test, target_os = "linux"))]
static ACTIVE_LINUX_SUPERVISORS: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(test, target_os = "linux"))]
struct ActiveLinuxSupervisor;

#[cfg(all(test, target_os = "linux"))]
impl ActiveLinuxSupervisor {
    fn enter() -> Self {
        ACTIVE_LINUX_SUPERVISORS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for ActiveLinuxSupervisor {
    fn drop(&mut self) {
        ACTIVE_LINUX_SUPERVISORS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Test-only acknowledgement that all Linux supervisors, including their
/// synchronous pipe-drain closure, have returned.
#[cfg(all(test, target_os = "linux"))]
fn active_linux_supervisor_count_for_test() -> usize {
    ACTIVE_LINUX_SUPERVISORS.load(Ordering::SeqCst)
}

/// The outcome of the status-wait phase.
#[cfg(target_os = "linux")]
enum StatusOutcome {
    /// Direct child terminated before the deadline and without caller
    /// cancellation.
    Received(TerminalStatus),
    /// The command timeout elapsed.
    DeadlineElapsed,
    /// The caller dropped the future / completion receiver.
    Cancelled,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct CleanupFailure {
    phase: CleanupPhase,
    direct_status: Option<TerminalStatus>,
}

#[cfg(target_os = "linux")]
fn cleanup_exhaustion_io(
    command_id: String,
    pgid: i32,
    failure: CleanupFailure,
    original: io::Error,
) -> io::Error {
    let kind = original.kind();
    io::Error::new(
        kind,
        CleanupExhaustionError::new(
            command_id,
            pgid,
            failure.direct_status,
            failure.phase,
            proc_group_members(pgid),
            original,
        ),
    )
}

#[cfg(target_os = "linux")]
fn original_command_error(timed_out: bool, status: Option<TerminalStatus>) -> io::Error {
    if timed_out {
        io::Error::new(io::ErrorKind::TimedOut, "process timed out")
    } else {
        io::Error::other(format!(
            "command ended before cleanup completed (direct status: {status:?})"
        ))
    }
}

/// Run the full Linux command supervision lifecycle on a blocking thread.
///
/// This function is the heart of the cancellation-independent supervisor.  It
/// never calls `Child::wait`, `try_wait`, or `wait_timeout`: the central reaper
/// owns all direct-child status consumption.  The `Child` handle is dropped
/// immediately after taking the pipe handles so no wait method can be invoked.
#[cfg(target_os = "linux")]
fn run_linux_supervisor(
    mut cmd: Command,
    timeout: Duration,
    cancel_rx: std::sync::mpsc::Receiver<()>,
) -> io::Result<(Output, bool)> {
    // Declare this before every other local so it drops last, after `drains`
    // and all pipe handles. This is an end-of-supervisor acknowledgement, not
    // merely an indication that the reaper delivered direct-child status.
    #[cfg(test)]
    let _active_supervisor = ActiveLinuxSupervisor::enter();
    let reaper = worker_child_reaper();
    let command_id = uuid::Uuid::now_v7().to_string();

    // ── 1. Reserve admission BEFORE spawn ───────────────────────────────
    // If admission fails (e.g. shutdown closed it), return the original io::Error
    // without spawning anything.
    let admission = reaper.admit(&command_id)?;

    // ── 2. Spawn ────────────────────────────────────────────────────────
    // Spawn errors remain the original io::Error.  The Admission is dropped
    // (releasing the reservation) if spawn fails.
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            admission.release();
            return Err(err);
        }
    };
    let pid = child.id();
    let pgid = pid as i32;

    // Take the pipe handles for drain threads.  We must NOT call any wait
    // method on `child` — the central reaper is the sole status consumer.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Drop the Child handle: we no longer need it.  `std::process::Child::drop`
    // does NOT kill the child, so the process continues normally.  The reaper
    // will observe its terminal status.
    drop(child);

    // Registration failure is post-spawn, so bounded cleanup owns the live group.
    let direct = match admission.register_direct(pid, pgid) {
        Ok(direct) => direct,
        Err(err) => {
            let cleanup = cleanup_unregistered_child(reaper, pgid, stdout, stderr);
            return match cleanup {
                Ok(()) => Err(err),
                Err(cleanup_err) => Err(cleanup_err),
            };
        }
    };

    // ── 4. Retain non-blocking pipe ownership in this supervisor ───────
    // An escaped descendant can retain these fds after the original PGID is
    // gone. Do not delegate reads to drain threads which could then outlive us.
    let mut drains = LinuxDrains::new(stdout, stderr);
    if let Err(err) = drains.set_nonblocking() {
        let cleanup = force_cleanup(&direct, pgid, &mut drains);
        let _ = drains.finish();
        let quiesced = reaper.quiesce(Duration::from_millis(100));
        if let Err(failure) = cleanup {
            return Err(cleanup_exhaustion_io(command_id, pgid, failure, err));
        }
        return Err(if quiesced {
            err
        } else {
            io::Error::new(io::ErrorKind::TimedOut, "child reaper did not quiesce")
        });
    }

    // ── 5. Wait for direct status / detect timeout / detect cancellation ─
    let clock = SystemClock::new();
    let deadline = clock.now_instant() + timeout;
    let outcome = poll_status_outcome(&direct, deadline, &cancel_rx, &mut drains);
    let deadline_fired = matches!(&outcome, StatusOutcome::DeadlineElapsed);

    let lifecycle = match outcome {
        StatusOutcome::Received(status) => {
            // Normal direct exit: give descendants up to 2 s of natural grace
            // to disappear on their own before escalating.
            if poll_pgid_gone(pgid, PHASE_TIMEOUT, &mut drains) {
                Ok((status, false))
            } else {
                // Descendants survived natural grace — escalate to TERM/KILL.
                // We already have the direct status, so we only need to ensure
                // the PGID is gone; no further status receive is needed.
                ensure_pgid_gone(pgid, status, &mut drains).map(|()| (status, false))
            }
        }
        StatusOutcome::DeadlineElapsed | StatusOutcome::Cancelled => {
            // Timeout or caller cancellation: skip natural grace and start
            // bounded TERM → KILL cleanup immediately.  We still need to
            // receive the direct status from the reaper.
            force_cleanup(&direct, pgid, &mut drains).map(|forced| (forced, true))
        }
    };

    // ── 6. Reaper quiescence pass ───────────────────────────────────────
    // Ensure the background waiter has processed any statuses queued by the
    // kernel but not yet observed by the registry.
    let quiesced = reaper.quiesce(Duration::from_millis(100));

    // ── 7. Collect available output and synchronously close pipes ───────
    // Never wait for EOF: escaped descendants may retain a pipe indefinitely.
    let (stdout_bytes, stderr_bytes) = drains.finish();

    let (status, timed_out) = match lifecycle {
        Ok(lifecycle) => lifecycle,
        Err(failure) => {
            let original = original_command_error(deadline_fired, failure.direct_status);
            return Err(cleanup_exhaustion_io(command_id, pgid, failure, original));
        }
    };
    if !quiesced {
        return Err(cleanup_exhaustion_io(
            command_id,
            pgid,
            CleanupFailure {
                phase: CleanupPhase::ReaperQuiescence,
                direct_status: Some(status),
            },
            original_command_error(timed_out, Some(status)),
        ));
    }

    // ── 8. Build and return output ──────────────────────────────────────
    let exit_status = status_from_raw(status.raw_status);
    Ok((
        Output {
            status: exit_status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        },
        timed_out,
    ))
}

#[cfg(target_os = "linux")]
struct LinuxDrains {
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
}

#[cfg(target_os = "linux")]
impl LinuxDrains {
    fn new(
        stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
    ) -> Self {
        Self {
            stdout,
            stderr,
            stdout_bytes: Vec::new(),
            stderr_bytes: Vec::new(),
        }
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        if let Some(pipe) = self.stdout.as_ref() {
            set_nonblocking(pipe)?;
        }
        if let Some(pipe) = self.stderr.as_ref() {
            set_nonblocking(pipe)?;
        }
        Ok(())
    }

    fn drain_available(&mut self) {
        if let Some(pipe) = self.stdout.as_mut() {
            drain_nonblocking(pipe, &mut self.stdout_bytes);
        }
        if let Some(pipe) = self.stderr.as_mut() {
            drain_nonblocking(pipe, &mut self.stderr_bytes);
        }
    }

    fn finish(mut self) -> (Vec<u8>, Vec<u8>) {
        self.drain_available();
        // Closing owned fds is synchronous; never wait for EOF from a
        // descendant which escaped the original process group.
        drop(self.stdout.take());
        drop(self.stderr.take());
        (self.stdout_bytes, self.stderr_bytes)
    }
}

#[cfg(target_os = "linux")]
fn set_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: the owned pipe remains valid for both fcntl calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: changing status flags does not consume or invalidate the fd.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn drain_nonblocking(pipe: &mut impl io::Read, bytes: &mut Vec<u8>) {
    let mut chunk = [0_u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return,
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => return,
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(ref err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Poll `kill(-pgid, 0)` until it returns `ESRCH` (process group gone) or the
/// timeout elapses.  Returns `true` if the group disappeared within the timeout.
#[cfg(target_os = "linux")]
fn poll_pgid_gone(pgid: i32, timeout: Duration, drains: &mut LinuxDrains) -> bool {
    let clock = SystemClock::new();
    let deadline = clock.now_instant() + timeout;
    loop {
        drains.drain_available();
        if pgid_is_gone(pgid) {
            return true;
        }
        if clock.now_instant() >= deadline {
            return false;
        }
        std::thread::sleep(PGID_POLL_INTERVAL);
    }
}

/// Whether the process group has disappeared (`kill(-pgid, 0)` → `ESRCH`).
#[cfg(target_os = "linux")]
fn pgid_is_gone(pgid: i32) -> bool {
    // SAFETY: `kill(-pgid, 0)` with signal 0 is a standard liveness probe —
    // it does not deliver any signal, just checks process existence.
    let rc = unsafe { libc::kill(-pgid, 0) };
    if rc == 0 {
        // Group is still alive.
        false
    } else {
        io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

#[cfg(target_os = "linux")]
fn cleanup_unregistered_child(
    reaper: &crate::child_reaper::ChildReaper,
    pgid: i32,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
) -> io::Result<()> {
    let _ = signal_process_group(pgid, libc::SIGTERM);
    if !poll_pgid_gone_without_drains(pgid, PHASE_TIMEOUT) {
        let _ = signal_process_group(pgid, libc::SIGKILL);
        if !poll_pgid_gone_without_drains(pgid, PHASE_TIMEOUT) {
            drop(stdout);
            drop(stderr);
            return Err(io::Error::other(format!(
                "process group {pgid} survived registration-failure cleanup"
            )));
        }
    }
    drop(stdout);
    drop(stderr);
    if reaper.quiesce(Duration::from_millis(100)) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "child reaper did not quiesce after registration failure",
        ))
    }
}

#[cfg(target_os = "linux")]
fn poll_pgid_gone_without_drains(pgid: i32, timeout: Duration) -> bool {
    let clock = SystemClock::new();
    let deadline = clock.now_instant() + timeout;
    loop {
        if pgid_is_gone(pgid) {
            return true;
        }
        if clock.now_instant() >= deadline {
            return false;
        }
        std::thread::sleep(PGID_POLL_INTERVAL);
    }
}

/// Ensure the process group is gone by escalating SIGTERM → SIGKILL if needed.
/// Used after the direct child's terminal status has already been received:
/// only the PGID disappearance matters, not further status receipt.
#[cfg(target_os = "linux")]
fn ensure_pgid_gone(
    pgid: i32,
    direct_status: TerminalStatus,
    drains: &mut LinuxDrains,
) -> Result<(), CleanupFailure> {
    let _ = signal_process_group(pgid, libc::SIGTERM);
    if poll_pgid_gone(pgid, PHASE_TIMEOUT, drains) {
        return Ok(());
    }
    let _ = signal_process_group(pgid, libc::SIGKILL);
    if poll_pgid_gone(pgid, PHASE_TIMEOUT, drains) {
        Ok(())
    } else {
        Err(CleanupFailure {
            phase: CleanupPhase::Kill,
            direct_status: Some(direct_status),
        })
    }
}

/// Bounded forced cleanup:
/// then escalate to `SIGKILL` for another `PHASE_TIMEOUT`.  Returns the
/// terminal status once obtained and the PGID has disappeared.
#[cfg(target_os = "linux")]
fn force_cleanup(
    direct: &DirectChild,
    pgid: i32,
    drains: &mut LinuxDrains,
) -> Result<TerminalStatus, CleanupFailure> {
    // First, get the direct status. It may arrive at any point during cleanup.
    // We'll poll for it while also signaling the group.
    let mut status: Option<TerminalStatus> = None;

    // Phase 1: SIGTERM
    let _ = signal_process_group(pgid, libc::SIGTERM);
    let clock = SystemClock::new();

    // Poll for status + PGID disappearance under TERM.
    let deadline = clock.now_instant() + PHASE_TIMEOUT;
    loop {
        drains.drain_available();
        if status.is_none()
            && let Ok(s) = direct.recv_timeout(Duration::ZERO)
        {
            status = Some(s);
        }
        if let Some(ref s) = status
            && pgid_is_gone(pgid)
        {
            return Ok(*s);
        }
        if clock.now_instant() >= deadline {
            break;
        }
        std::thread::sleep(PGID_POLL_INTERVAL);
    }

    // Phase 2: SIGKILL
    let _ = signal_process_group(pgid, libc::SIGKILL);
    let deadline = clock.now_instant() + PHASE_TIMEOUT;
    loop {
        drains.drain_available();
        if status.is_none()
            && let Ok(s) = direct.recv_timeout(Duration::ZERO)
        {
            status = Some(s);
        }
        if let Some(ref s) = status
            && pgid_is_gone(pgid)
        {
            return Ok(*s);
        }
        if clock.now_instant() >= deadline {
            break;
        }
        std::thread::sleep(PGID_POLL_INTERVAL);
    }

    Err(CleanupFailure {
        phase: CleanupPhase::Kill,
        direct_status: status,
    })
}

/// Snapshot PIDs which still have `pgid` as their process-group ID. A process
/// can disappear between directory enumeration and stat read; that race is
/// intentionally ignored, so `ESRCH`/missing `/proc` entries are never reported
/// as survivors.
#[cfg(target_os = "linux")]
fn proc_group_members(pgid: i32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut survivors = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            // `comm` may contain spaces and parentheses. Everything after its
            // final ')' begins with state, ppid, then pgrp (field 5).
            let Some((_, after_comm)) = stat.rsplit_once(") ") else {
                return false;
            };
            after_comm
                .split_whitespace()
                .nth(2)
                .and_then(|field| field.parse::<i32>().ok())
                == Some(pgid)
        })
        .collect::<Vec<_>>();
    survivors.sort_unstable();
    survivors
}

/// Race direct-status receipt against the command deadline and caller
/// cancellation.  Polls at ~25 ms resolution.
#[cfg(target_os = "linux")]
fn poll_status_outcome(
    direct: &DirectChild,
    deadline: Instant,
    cancel_rx: &std::sync::mpsc::Receiver<()>,
    drains: &mut LinuxDrains,
) -> StatusOutcome {
    use std::sync::mpsc::TryRecvError;
    let clock = SystemClock::new();
    loop {
        drains.drain_available();
        // Try non-blocking receive on the direct status channel.
        if let Ok(status) = direct.recv_timeout(Duration::ZERO) {
            return StatusOutcome::Received(status);
        }

        // Check for caller cancellation (non-blocking).
        if let Err(TryRecvError::Disconnected) = cancel_rx.try_recv() {
            return StatusOutcome::Cancelled;
        }

        // Check deadline.
        let now = clock.now_instant();
        if now >= deadline {
            return StatusOutcome::DeadlineElapsed;
        }

        // Sleep for the poll interval (or remaining time, whichever is shorter)
        // before the next iteration.
        let remaining = deadline - now;
        let wait = remaining.min(PGID_POLL_INTERVAL);
        std::thread::sleep(wait);
    }
}

// ===========================================================================
// Non-Linux Unix: legacy wait_timeout path (unchanged behaviour)
// ===========================================================================

/// Run `cmd` on a blocking thread, killing its entire process group if it does
/// not finish within `timeout`.
///
/// The kill escalates SIGTERM → (200ms grace) → SIGKILL, and stdout/stderr are
/// drained on background threads to avoid the classic full-pipe deadlock.
#[cfg(all(unix, not(target_os = "linux")))]
pub async fn output_with_kill(cmd: Command, timeout: Duration) -> io::Result<Output> {
    output_with_kill_inner(cmd, timeout)
        .await
        .map(|(out, _timed_out)| out)
}

/// Inner form of [`output_with_kill`] for non-Linux Unix.  Uses `wait_timeout`
/// for deadline-based child status.
#[cfg(all(unix, not(target_os = "linux")))]
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

// ===========================================================================
// Non-Unix: plain spawn_blocking (no process-group isolation)
// ===========================================================================

#[cfg(not(unix))]
pub async fn output_with_kill(mut cmd: Command, _timeout: Duration) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .map_err(io::Error::other)?
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use djinn_core::clock::{Clock, SystemClock};

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_exhaustion_preserves_all_diagnostic_fields_and_source_chain() {
        let direct_status = TerminalStatus {
            pid: 41,
            raw_status: 9,
        };
        let original = io::Error::new(io::ErrorKind::TimedOut, "process timed out");
        let err = io::Error::new(
            io::ErrorKind::TimedOut,
            CleanupExhaustionError::new(
                "command-42".to_owned(),
                42,
                Some(direct_status),
                CleanupPhase::Kill,
                vec![41, 43],
                original,
            ),
        );

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let diagnostic = err
            .get_ref()
            .and_then(|source| source.downcast_ref::<CleanupExhaustionError>())
            .expect("io error must retain typed cleanup diagnostic");
        assert_eq!(diagnostic.command_id, "command-42");
        assert_eq!(diagnostic.pgid, 42);
        assert_eq!(diagnostic.direct_status, Some(direct_status));
        assert_eq!(diagnostic.phase, CleanupPhase::Kill);
        assert_eq!(diagnostic.surviving_pids, vec![41, 43]);

        let typed_source = diagnostic.source().expect("original error source");
        let timeout = typed_source
            .downcast_ref::<io::Error>()
            .expect("original timeout error must remain in source chain");
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        assert!(timeout.source().is_none());
    }

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
        // 200ms timeout + bounded TERM/KILL cleanup; comfortably under 8s.
        assert!(
            elapsed < Duration::from_secs(8),
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

        // The supervisor should have already ensured the direct child's process
        // group was cleaned up.  On systems where PID 1 reaps orphans promptly,
        // kill(-pgid, 0) returns ESRCH.  In test containers where PID 1 is not a
        // reaping init, orphaned grandchildren may linger as zombies; verify that
        // no *live* (non-zombie) member of the group remains.
        let mut gone = false;
        for _ in 0..20 {
            let rc = unsafe { libc::kill(-pgid, 0) };
            if rc != 0 {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !gone {
            // The group didn't fully disappear — check that any remaining
            // processes are zombies (state Z), not live.
            let live = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "ps -e -o pgid=,stat= --no-headers | awk '$1=={pgid} && $2!~/Z/' | wc -l"
                ))
                .output();
            if let Ok(out) = live {
                let count: usize = String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse()
                    .unwrap_or(0);
                assert_eq!(
                    count, 0,
                    "live processes remain in process group {pgid} after timeout kill"
                );
            }
        }
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

// ===========================================================================

#[cfg(all(test, target_os = "linux"))]
#[path = "process_linux_supervisor_tests.rs"]
mod linux_supervisor_tests;
