//! Async process spawning via `std::process::Command` + `spawn_blocking`.
//!
//! All subprocess creation in the daemon MUST go through this module rather than
//! using `tokio::process::Command` directly.  The tokio process driver registers
//! child PIDs with the async reactor (kqueue on macOS), and the reactor fd can
//! become stale when the server runs as a background daemon with null stdio,
//! causing every subsequent spawn to fail with EBADF (os error 9).
//!
//! `std::process::Command` avoids this by not touching the reactor at all.

use std::future::Future;
use std::io;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

use djinn_cgroup_launcher::CpuStat;
use djinn_core::clock::{Clock, SystemClock};
use djinn_supervisor::services::{
    LeaseAbandonRequest, LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest, LeaseIdentity,
    LeaseQueueRequest, LeaseReleaseRequest, LeaseResult, LeaseState, LeaseStatusRequest,
    SupervisorServices, TaskInvocationLeaseIdentity,
};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[cfg(unix)]
use wait_timeout::ChildExt;

/// OOM score adjustment for agent child processes.  Higher values make the
/// kernel OOM-killer prefer these processes over the rest of the system.
#[cfg(target_os = "linux")]
const CHILD_OOM_SCORE_ADJ: &str = "800\n";

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
            let _ = libc::setpriority(libc::PRIO_PROCESS, 0, 10);

            // I/O priority: best-effort class (2) with lowest priority (7).
            // ioprio_set is not in libc, use raw syscall.
            #[cfg(target_os = "linux")]
            {
                const IOPRIO_WHO_PROCESS: i32 = 1;
                const IOPRIO_CLASS_BE: i32 = 2;
                // Encoding: (class << 13) | level
                let ioprio_val = (IOPRIO_CLASS_BE << 13) | 7;
                let _ = libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio_val);

                // Raise OOM score so the kernel prefers killing agent children
                // over the user's desktop processes.
                let _ = std::fs::write("/proc/self/oom_score_adj", CHILD_OOM_SCORE_ADJ);
            }

            Ok(())
        });
    }
}

/// The launcher is deliberately separate from the supervisor: it controls one
/// cgroup leaf and samples `cpu.stat`; it never owns a second lease ledger.
pub(crate) trait CgroupLauncherClient: Send + Sync + 'static {
    fn launch(
        &self,
        command: &mut Command,
        identity: &TaskInvocationLeaseIdentity,
    ) -> io::Result<std::process::Child>;
    fn sample_cpu(&self, identity: &TaskInvocationLeaseIdentity) -> io::Result<CpuStat>;
    fn fenced_lift(
        &self,
        identity: &TaskInvocationLeaseIdentity,
        fence: &LeaseFencingToken,
    ) -> io::Result<()>;
    fn kill(&self, identity: &TaskInvocationLeaseIdentity) -> io::Result<()>;
    fn wait_empty(&self, identity: &TaskInvocationLeaseIdentity) -> io::Result<()>;
}

#[derive(Clone, Debug)]
pub(crate) struct LeaseInvocationConfig {
    pub task_id: String,
    pub task_run_id: String,
    pub cpu_usage_threshold_usec: u64,
    pub queue_deadline_ms: i64,
    pub launch_deadline_ms: i64,
    pub timeout: Duration,
}

#[derive(Debug)]
pub(crate) enum LeaseInvocationError {
    Process(ProcessRunError),
    Launcher(io::Error),
    LeaseIdentityConflict,
    LeaseWaitTimeout,
    LeaseUnavailable,
}

#[derive(Debug)]
pub(crate) struct LeaseInvocationOutput {
    pub(crate) process: ProcessOutput,
    pub(crate) identity: TaskInvocationLeaseIdentity,
}

/// Launcher-backed invocation state machine. A queue is issued only after a
/// measured CPU threshold; terminal intent is set before cgroup kill/reconcile.
pub(crate) struct LeaseInvocationRunner {
    services: Arc<dyn SupervisorServices>,
    launcher: Arc<dyn CgroupLauncherClient>,
    clock: Arc<dyn Clock>,
}
impl LeaseInvocationRunner {
    pub(crate) fn new(
        services: Arc<dyn SupervisorServices>,
        launcher: Arc<dyn CgroupLauncherClient>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            services,
            launcher,
            clock,
        }
    }
    pub(crate) async fn output(
        &self,
        mut cmd: Command,
        config: LeaseInvocationConfig,
        cancel: CancellationToken,
    ) -> Result<LeaseInvocationOutput, LeaseInvocationError> {
        let identity = TaskInvocationLeaseIdentity {
            task_id: config.task_id,
            task_run_id: config.task_run_id,
            invocation_id: uuid::Uuid::now_v7().to_string(),
        };
        let lease = LeaseIdentity::TaskInvocation(identity.clone());
        let mut child = self
            .launcher
            .launch(&mut cmd, &identity)
            .map_err(LeaseInvocationError::Launcher)?;
        let stdout = spawn_drain(child.stdout.take());
        let stderr = spawn_stderr_drain(child.stderr.take());
        let mut deadline = self.clock.now_instant() + config.timeout;
        let (mut queued, mut fence, mut credit_used, mut lease_error) = (false, None, false, None);
        // This variable is assigned only by `terminal_now` or by a service
        // wait that observed terminal intent. Consequently no later response
        // can authorize a cgroup lift.
        let (observed, termination) = 'invocation: loop {
            if let Some(terminal) =
                terminal_now(&mut child, &cancel, self.clock.as_ref(), deadline)?
            {
                break terminal;
            }
            let cpu = self
                .launcher
                .sample_cpu(&identity)
                .map_err(LeaseInvocationError::Launcher)?;
            let result = if !queued && cpu.usage_usec >= config.cpu_usage_threshold_usec {
                queued = true;
                match await_lease_or_terminal(
                    self.services.queue_lease(LeaseQueueRequest {
                        identity: lease.clone(),
                        deadlines: LeaseDeadlines {
                            queue_deadline_ms: config.queue_deadline_ms,
                            launch_deadline_ms: config.launch_deadline_ms,
                        },
                    }),
                    &mut child,
                    &cancel,
                    self.clock.as_ref(),
                    deadline,
                )
                .await?
                {
                    LeaseWait::Response(value) => value,
                    LeaseWait::Terminal(terminal) => break 'invocation terminal,
                }
            } else if queued {
                match await_lease_or_terminal(
                    self.services.lease_status(LeaseStatusRequest {
                        identity: lease.clone(),
                    }),
                    &mut child,
                    &cancel,
                    self.clock.as_ref(),
                    deadline,
                )
                .await?
                {
                    LeaseWait::Response(value) => value,
                    LeaseWait::Terminal(terminal) => break 'invocation terminal,
                }
            } else {
                // Do not use Tokio wall-clock timers here: tests advance the
                // injected Clock while a fake launcher/service is paused.
                tokio::task::yield_now().await;
                continue;
            };
            let grant = match result {
                LeaseResult::Granted(grant) => Some(grant.fencing_token),
                LeaseResult::Status(status) if status.state == LeaseState::Granted => {
                    status.fencing_token
                }
                other => {
                    lease_failure(other, &mut deadline, &mut credit_used, &mut lease_error);
                    None
                }
            };
            if let Some(token) = grant {
                let durable = match await_lease_or_terminal(
                    self.services.grant_lease(LeaseGrantRequest {
                        identity: lease.clone(),
                        fencing_token: token.clone(),
                    }),
                    &mut child,
                    &cancel,
                    self.clock.as_ref(),
                    deadline,
                )
                .await?
                {
                    LeaseWait::Response(value) => value,
                    LeaseWait::Terminal(terminal) => break 'invocation terminal,
                };
                // A grant response is an acknowledgement, not authorization:
                // only a matching still-live durable state permits lift.
                match durable {
                    LeaseResult::Status(status)
                        if matches!(
                            status.state,
                            LeaseState::Launching | LeaseState::Bound | LeaseState::Active
                        ) && status.fencing_token.as_ref() == Some(&token) =>
                    {
                        if let Some(terminal) =
                            terminal_now(&mut child, &cancel, self.clock.as_ref(), deadline)?
                        {
                            break 'invocation terminal;
                        }
                        self.launcher
                            .fenced_lift(&identity, &token)
                            .map_err(LeaseInvocationError::Launcher)?;
                        fence = Some(token);
                    }
                    other => {
                        lease_failure(other, &mut deadline, &mut credit_used, &mut lease_error)
                    }
                }
            }
            if lease_error.is_some() {
                break (
                    child.try_wait().map_err(started_lease_error)?,
                    ProcessTermination::Cancelled,
                );
            }
            tokio::task::yield_now().await;
        };
        self.launcher
            .kill(&identity)
            .map_err(LeaseInvocationError::Launcher)?;
        self.launcher
            .wait_empty(&identity)
            .map_err(LeaseInvocationError::Launcher)?;
        let status = observed.unwrap_or(child.wait().map_err(started_lease_error)?);
        let process = ProcessOutput {
            output: Output {
                status,
                stdout: join_with_timeout(stdout, Duration::from_secs(2)),
                stderr: join_with_timeout(stderr, Duration::from_secs(2)),
            },
            termination,
        };
        if queued {
            reconcile_terminal_lease(self.services.as_ref(), lease, fence).await;
        }
        if let Some(error) = lease_error {
            Err(error)
        } else {
            Ok(LeaseInvocationOutput { process, identity })
        }
    }
}
/// Result of waiting for a supervisor response. A terminal observation wins
/// over a delayed response and permanently closes the lift path.
enum LeaseWait {
    Response(LeaseResult),
    Terminal((Option<std::process::ExitStatus>, ProcessTermination)),
}

async fn await_lease_or_terminal<F>(
    request: F,
    child: &mut std::process::Child,
    cancel: &CancellationToken,
    clock: &dyn Clock,
    deadline: std::time::Instant,
) -> Result<LeaseWait, LeaseInvocationError>
where
    F: Future<Output = LeaseResult>,
{
    tokio::pin!(request);
    loop {
        if let Some(terminal) = terminal_now(child, cancel, clock, deadline)? {
            return Ok(LeaseWait::Terminal(terminal));
        }
        tokio::select! {
            result = &mut request => return Ok(LeaseWait::Response(result)),
            _ = cancel.cancelled() => return Ok(LeaseWait::Terminal((child.try_wait().map_err(started_lease_error)?, ProcessTermination::Cancelled))),
            _ = tokio::task::yield_now() => {}
        }
    }
}

fn terminal_now(
    child: &mut std::process::Child,
    cancel: &CancellationToken,
    clock: &dyn Clock,
    deadline: std::time::Instant,
) -> Result<Option<(Option<std::process::ExitStatus>, ProcessTermination)>, LeaseInvocationError> {
    if cancel.is_cancelled() {
        return Ok(Some((
            child.try_wait().map_err(started_lease_error)?,
            ProcessTermination::Cancelled,
        )));
    }
    if clock.now_instant() >= deadline {
        return Ok(Some((
            child.try_wait().map_err(started_lease_error)?,
            ProcessTermination::TimedOut,
        )));
    }
    Ok(child
        .try_wait()
        .map_err(started_lease_error)?
        .map(|status| (Some(status), ProcessTermination::Exited)))
}

/// Re-read durable state after every uncertain cleanup response. Operations
/// are idempotent and reuse the same fence, so a lost request cannot cause a
/// second capacity return or leave a known grant unreleased.
async fn reconcile_terminal_lease(
    services: &dyn SupervisorServices,
    lease: LeaseIdentity,
    observed_fence: Option<LeaseFencingToken>,
) {
    const RECONCILIATION_ATTEMPTS: usize = 3;
    let mut fence = observed_fence;
    for _ in 0..RECONCILIATION_ATTEMPTS {
        match services
            .lease_status(LeaseStatusRequest {
                identity: lease.clone(),
            })
            .await
        {
            LeaseResult::Status(status)
                if matches!(status.state, LeaseState::Cancelled | LeaseState::Released) =>
            {
                return;
            }
            LeaseResult::Status(status)
                if matches!(
                    status.state,
                    LeaseState::Granted
                        | LeaseState::Launching
                        | LeaseState::Bound
                        | LeaseState::Active
                ) =>
            {
                fence = status.fencing_token.or(fence)
            }
            LeaseResult::Cancelled { .. }
            | LeaseResult::Released { .. }
            | LeaseResult::Abandoned { .. } => return,
            _ => {}
        }
        let result = if let Some(token) = fence.clone() {
            services
                .release_lease(LeaseReleaseRequest {
                    identity: lease.clone(),
                    fencing_token: token,
                    candidate_cleanup: false,
                })
                .await
        } else {
            services
                .abandon_lease(LeaseAbandonRequest {
                    identity: lease.clone(),
                    candidate_cleanup: false,
                })
                .await
        };
        if matches!(
            result,
            LeaseResult::Released { .. }
                | LeaseResult::Cancelled { .. }
                | LeaseResult::Abandoned { .. }
        ) {
            return;
        }
        tokio::task::yield_now().await;
    }
}

fn started_lease_error(error: io::Error) -> LeaseInvocationError {
    LeaseInvocationError::Process(ProcessRunError::Started(error))
}
fn lease_failure(
    result: LeaseResult,
    deadline: &mut std::time::Instant,
    credit_used: &mut bool,
    output: &mut Option<LeaseInvocationError>,
) {
    match result {
        LeaseResult::LeaseIdentityConflict { .. } => {
            *output = Some(LeaseInvocationError::LeaseIdentityConflict)
        }
        LeaseResult::LeaseUnavailable => *output = Some(LeaseInvocationError::LeaseUnavailable),
        LeaseResult::LeaseWaitTimeout {
            timeout_credit: Some(credit),
        } if !*credit_used => {
            *deadline += Duration::from_millis(u64::from(credit.retry_after_ms));
            *credit_used = true;
        }
        LeaseResult::LeaseWaitTimeout { .. } => {
            *output = Some(LeaseInvocationError::LeaseWaitTimeout)
        }
        _ => {}
    }
}

#[cfg(not(unix))]
pub fn isolate_process_group(_cmd: &mut Command) {}

/// How a process run finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessTermination {
    /// The child exited on its own (success or failure).
    Exited,
    /// The timeout deadline fired and the child was terminated/reaped.
    TimedOut,
    /// Cancellation was requested and the child was terminated/reaped.
    Cancelled,
}

/// Output of a process run plus the terminal reason.
#[derive(Debug)]
pub(crate) struct ProcessOutput {
    pub(crate) output: Output,
    #[allow(dead_code)] // consumed by T2/T3 telemetry wiring
    pub(crate) termination: ProcessTermination,
}

/// Error from a process run.
///
/// `Spawn` means the child never started; `Started` means the child did start
/// but wait/reap failed or the `spawn_blocking` task panicked.
#[derive(Debug)]
pub(crate) enum ProcessRunError {
    Spawn(io::Error),
    Started(io::Error),
}

impl ProcessRunError {
    /// Flatten back to `io::Error` for the legacy compatibility API.
    pub(crate) fn into_io_error(self) -> io::Error {
        match self {
            ProcessRunError::Spawn(e) | ProcessRunError::Started(e) => e,
        }
    }
}

/// Join a drain thread with a wall-clock deadline.
///
/// If the thread doesn't finish in time (e.g. a surviving subprocess still
/// holds the pipe open), we abandon it and return whatever bytes it collected
/// up to that point — or an empty vec if it never finished.
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

/// Spawn a thread that drains an optional pipe into a buffer.
fn spawn_drain(stream: Option<std::process::ChildStdout>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stream {
            let _ = io::Read::read_to_end(&mut out, &mut buf);
        }
        buf
    })
}

fn spawn_stderr_drain(
    stream: Option<std::process::ChildStderr>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut err) = stream {
            let _ = io::Read::read_to_end(&mut err, &mut buf);
        }
        buf
    })
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
fn cleanup_and_reap(
    child: &mut std::process::Child,
    pgid: i32,
) -> io::Result<std::process::ExitStatus> {
    let _ = signal_process_group(pgid, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(200));

    if child.try_wait()?.is_none() {
        let _ = signal_process_group(pgid, libc::SIGKILL);
    }

    // Reap the child. On the normal path this returns immediately. On the
    // timed-out/cancelled path the SIGKILL above usually reaps it within
    // milliseconds — but a child stuck in uninterruptible sleep (D state),
    // e.g. a `git` blocked on a hung network read, cannot be killed until
    // that IO unblocks. An unbounded `wait()` there blocks this tool call
    // FOREVER: the worker hangs, the tool heartbeat keeps the session
    // "alive" so neither stall reaper fires, and the Pod wastes its slot
    // until the 1h activeDeadline. Bound the post-kill reap: if the child
    // will not die within the grace, abandon it (a zombie bounded by the
    // Pod activeDeadline) and report a SIGKILL exit so the worker unblocks
    // and can recover instead of hanging indefinitely.
    const POST_KILL_REAP_GRACE: Duration = Duration::from_secs(3);
    match child.wait_timeout(POST_KILL_REAP_GRACE)? {
        Some(status) => Ok(status),
        None => {
            tracing::warn!(
                pgid,
                "output_with_kill: child survived SIGKILL after timeout/cancel \
                 (likely uninterruptible IO); abandoning to unblock the worker"
            );
            Ok(std::process::ExitStatus::from_raw(libc::SIGKILL))
        }
    }
}

/// Richer, cancellable runner.
///
/// The blocking closure owns the `std::process::Child` and polls cancellation
/// between short waits. Timeout and cancellation both run the same
/// process-group TERM, grace, KILL, bounded reap, and pipe-drain cleanup
/// before returning.
#[cfg(unix)]
pub(crate) async fn output_with_kill_cancellable(
    mut cmd: Command,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<ProcessOutput, ProcessRunError> {
    tokio::task::spawn_blocking(move || {
        let start = SystemClock::new().now_instant();
        let deadline = start + timeout;
        const POLL_INTERVAL: Duration = Duration::from_millis(50);

        let mut child = cmd.spawn().map_err(ProcessRunError::Spawn)?;
        let pgid = child.id() as i32;

        // Drain stdout and stderr in background threads to prevent pipe buffer
        // deadlock. The Linux pipe buffer is 64KB — if the child writes more
        // than that before we read, it blocks on write() and wait_timeout()
        // never returns (classic pipe deadlock).
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_handle = spawn_drain(stdout);
        let stderr_handle = spawn_stderr_drain(stderr);

        let (status, termination) = loop {
            if cancel.is_cancelled() {
                let status =
                    cleanup_and_reap(&mut child, pgid).map_err(ProcessRunError::Started)?;
                break (status, ProcessTermination::Cancelled);
            }

            let now = SystemClock::new().now_instant();
            if now >= deadline {
                let status =
                    cleanup_and_reap(&mut child, pgid).map_err(ProcessRunError::Started)?;
                break (status, ProcessTermination::TimedOut);
            }

            let remaining = deadline - now;
            let wait_for = remaining.min(POLL_INTERVAL);
            match child
                .wait_timeout(wait_for)
                .map_err(ProcessRunError::Started)?
            {
                Some(status) => break (status, ProcessTermination::Exited),
                None => continue,
            }
        };

        // After a kill the drain threads may block forever if any subprocess
        // survived in a different process group and still holds the pipe open.
        // Give them a short deadline; take whatever bytes arrived before it.
        let killed = termination != ProcessTermination::Exited;
        let drain_deadline = Duration::from_secs(if killed { 2 } else { 60 });
        let stdout_bytes = join_with_timeout(stdout_handle, drain_deadline);
        let stderr_bytes = join_with_timeout(stderr_handle, drain_deadline);

        Ok(ProcessOutput {
            output: Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            },
            termination,
        })
    })
    .await
    .map_err(|e| ProcessRunError::Started(io::Error::other(e)))?
}

#[cfg(not(unix))]
pub(crate) async fn output_with_kill_cancellable(
    mut cmd: Command,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<ProcessOutput, ProcessRunError> {
    tokio::task::spawn_blocking(move || {
        let start = SystemClock::new().now_instant();
        let deadline = start + timeout;
        const POLL_INTERVAL: Duration = Duration::from_millis(50);

        let mut child = cmd.spawn().map_err(ProcessRunError::Spawn)?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_handle = spawn_drain(stdout);
        let stderr_handle = spawn_stderr_drain(stderr);

        #[derive(Clone, Copy)]
        enum KillReason {
            Cancel,
            Timeout,
        }

        let mut status = None;
        let mut kill_reason = None;

        while status.is_none() {
            if cancel.is_cancelled() {
                let _ = child.kill();
                kill_reason = Some(KillReason::Cancel);
                break;
            }

            let now = SystemClock::new().now_instant();
            if now >= deadline {
                let _ = child.kill();
                kill_reason = Some(KillReason::Timeout);
                break;
            }

            status = child.try_wait().map_err(ProcessRunError::Started)?;
            if status.is_none() {
                let remaining = deadline - now;
                std::thread::sleep(POLL_INTERVAL.min(remaining));
            }
        }

        let status = match status {
            Some(status) => status,
            None => child.wait().map_err(ProcessRunError::Started)?,
        };

        let termination = match kill_reason {
            Some(KillReason::Cancel) => ProcessTermination::Cancelled,
            Some(KillReason::Timeout) => ProcessTermination::TimedOut,
            None => ProcessTermination::Exited,
        };

        let drain_deadline = Duration::from_secs(if kill_reason.is_some() { 2 } else { 60 });
        let stdout_bytes = join_with_timeout(stdout_handle, drain_deadline);
        let stderr_bytes = join_with_timeout(stderr_handle, drain_deadline);

        Ok(ProcessOutput {
            output: Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            },
            termination,
        })
    })
    .await
    .map_err(|e| ProcessRunError::Started(io::Error::other(e)))?
}

/// Legacy compatibility wrapper for Unix callers.
///
/// This preserves the original timeout-and-process-group cleanup behavior while
/// discarding the richer terminal reason.
#[cfg(unix)]
pub async fn output_with_kill(cmd: Command, timeout: Duration) -> io::Result<Output> {
    output_with_kill_cancellable(cmd, timeout, CancellationToken::new())
        .await
        .map(|po| po.output)
        .map_err(|e| e.into_io_error())
}

/// Legacy compatibility wrapper for non-Unix callers.
///
/// Historically this platform used `Command::output` in a blocking task and
/// ignored `timeout`. Keep that behavior for existing production callers. The
/// richer cancellable runner above intentionally has direct-child timeout and
/// cancellation semantics for new internal consumers, but must not alter this
/// compatibility API.
#[cfg(not(unix))]
pub async fn output_with_kill(mut cmd: Command, _timeout: Duration) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || cmd.output())
        .await
        .map_err(io::Error::other)?
}

#[cfg(all(test, unix))]
#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests use real time for timeout/duration assertions
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawned_process_uses_different_pgid() {
        let parent_pgid = unsafe { libc::getpgrp() };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf '%d' \"$(ps -o pgid= -p $$)\"");
        // output_with_kill drains stdout/stderr from the child's pipe handles;
        // those handles only exist when the caller opted into piping.
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        isolate_process_group(&mut cmd);

        let output = output_with_kill(cmd, Duration::from_secs(10))
            .await
            .expect("spawn succeeds");
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

    /// A child that ignores SIGTERM forces the SIGKILL escalation and the
    /// bounded post-kill reap. `output_with_kill` must still return promptly —
    /// the regression it guards is the old unbounded `child.wait()` that hung
    /// FOREVER on a child that would not die (e.g. a `git` in uninterruptible
    /// IO), wedging the worker and masking it from the stall reapers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_returns_promptly_even_when_sigterm_ignored() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("trap '' TERM; sleep 999");
        isolate_process_group(&mut cmd);

        let started = std::time::Instant::now();
        let out = output_with_kill(cmd, Duration::from_millis(100))
            .await
            .expect("must return after the timeout kill, not hang");
        assert!(!out.status.success());
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "output_with_kill must return in bounded time after a timeout kill, got {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellable_exits_reports_exited() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hello");
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        isolate_process_group(&mut cmd);

        let cancel = CancellationToken::new();
        let result = output_with_kill_cancellable(cmd, Duration::from_secs(10), cancel)
            .await
            .expect("spawn succeeds");
        assert!(result.output.status.success());
        assert_eq!(result.termination, ProcessTermination::Exited);
        assert!(String::from_utf8_lossy(&result.output.stdout).contains("hello"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellable_spawn_failure_reports_spawn() {
        let mut cmd = Command::new("/definitely/does/not/exist");
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let cancel = CancellationToken::new();
        let result = output_with_kill_cancellable(cmd, Duration::from_secs(10), cancel).await;
        assert!(
            matches!(result, Err(ProcessRunError::Spawn(_))),
            "expected spawn error, got {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellable_timeout_reports_timed_out() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 999");
        isolate_process_group(&mut cmd);

        let cancel = CancellationToken::new();
        let result = output_with_kill_cancellable(cmd, Duration::from_millis(100), cancel)
            .await
            .expect("process should be reaped after timeout");
        assert!(!result.output.status.success());
        assert_eq!(result.termination, ProcessTermination::TimedOut);
    }

    /// Parse the process group from `/proc/{pid}/stat`.
    fn process_group_of(pid: i32) -> Option<i32> {
        let path = format!("/proc/{pid}/stat");
        let contents = std::fs::read_to_string(path).ok()?;
        let end = contents.find(')')?;
        let after_comm = &contents[end + 1..];
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // Format after comm: state ppid pgrp session ...
        fields.get(2)?.parse().ok()
    }

    /// Read the one-character state from `/proc/{pid}/stat`. `None` means the
    /// process no longer has a proc entry (fully reaped).
    fn process_state(pid: i32) -> Option<char> {
        let path = format!("/proc/{pid}/stat");
        let contents = std::fs::read_to_string(path).ok()?;
        let end = contents.find(')')?;
        let after_comm = &contents[end + 1..];
        after_comm.split_whitespace().next()?.chars().next()
    }

    /// Returns true if the process is still alive (not a zombie or reaped).
    fn is_process_running(pid: i32) -> bool {
        match process_state(pid) {
            None | Some('Z') => false,
            Some(_) => true,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellable_cancellation_reports_cancelled_and_kills_group() {
        let pid_file = tempfile::NamedTempFile::new().expect("temp file");
        let pid_path = pid_file.path().to_path_buf();
        let pid_path_str = pid_path.to_str().expect("path is utf8").to_string();

        // The shell is the direct child and the process group leader; the
        // background `sleep` is in the same group. When cancellation fires the
        // whole group must be terminated, not just the owning future dropped.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "sleep 999 & echo $$ > {pid_path_str}; echo $! >> {pid_path_str}; wait"
        ));
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        isolate_process_group(&mut cmd);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = tokio::spawn(output_with_kill_cancellable(
            cmd,
            Duration::from_secs(600),
            cancel_clone,
        ));

        // Wait for the shell to have written both its PID and the sleep PID.
        let pids = tokio::task::spawn_blocking(move || {
            let start = std::time::Instant::now();
            loop {
                if let Ok(content) = std::fs::read_to_string(&pid_path) {
                    let lines: Vec<&str> = content.lines().collect();
                    if lines.len() >= 2
                        && let (Ok(s), Ok(p)) = (lines[0].trim().parse(), lines[1].trim().parse())
                    {
                        return Some((s, p));
                    }
                }
                if start.elapsed() > Duration::from_secs(5) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .await
        .expect("blocking task completes");

        let (shell_pid, sleep_pid) = pids.expect("shell and sleep pids were written");

        // Verify the sleep is actually in the shell's process group before we cancel.
        let sleep_pgid = process_group_of(sleep_pid);
        assert_eq!(
            sleep_pgid,
            Some(shell_pid),
            "sleep {sleep_pid} should be in the shell's process group {shell_pid}"
        );

        // Cancel from the outside and wait for the runner to finish cleanup.
        cancel.cancel();
        let result = handle
            .await
            .expect("join succeeds")
            .expect("process should be reaped after cancellation");
        assert!(!result.output.status.success());
        assert_eq!(result.termination, ProcessTermination::Cancelled);

        // The sleep in the process group must be gone or a zombie, not still running.
        let sleep_alive = is_process_running(sleep_pid);
        assert!(
            !sleep_alive,
            "sleep {sleep_pid} in group {shell_pid} should have been killed by cancellation"
        );
    }
}
