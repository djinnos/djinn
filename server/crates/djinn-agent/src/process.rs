// djinn:allow-oversize — cohesive process lifecycle and durable lease recovery
// require shared private state; split only with a dedicated module boundary.
//! Async process spawning via `std::process::Command` + `spawn_blocking`.
//!
//! All subprocess creation in the daemon MUST go through this module rather than
//! using `tokio::process::Command` directly.  The tokio process driver registers
//! child PIDs with the async reactor (kqueue on macOS), and the reactor fd can
//! become stale when the server runs as a background daemon with null stdio,
//! causing every subsequent spawn to fail with EBADF (os error 9).
//!
//! `std::process::Command` avoids this by not touching the reactor at all.

use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use djinn_core::clock::{Clock, SystemClock};
use djinn_supervisor::services::{
    InvocationLiftAuthority, InvocationLiftDecision, LeaseAbandonRequest, LeaseBindRequest,
    LeaseDeadlines, LeaseFencingToken, LeaseGrantRequest, LeaseIdentity, LeaseQueueRequest,
    LeaseReleaseRequest, LeaseResult, LeaseState, LeaseStatus, LeaseStatusRequest,
    SupervisorServices, TaskInvocationLeaseIdentity, WatchdogTerminationRequest,
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

#[path = "process_broker.rs"]
mod broker;
/// The `Command` -> `CommandSpec` conversion the production broker adapter
/// uses. Exported so a proof can drive the REAL conversion from the REAL
/// caller instead of hand-assembling a spec — the seventh launcher blocker was
/// invisible precisely because every broker-backed test used a launcher that
/// ignored the `Command` it was handed.
#[cfg(test)]
pub(crate) use broker::command_spec;
/// Process-group containment for the launcher doubles' fixture children — the
/// stand-in for the leaf cgroup a bare `std::process::Child` does not have. See
/// [`broker::fixture_child`] for why a double that skips it leaks a process.
#[cfg(test)]
pub(crate) use broker::fixture_child;
/// The SINGLE source of an invocation's broker fence. Exported so a proof reads
/// the same function the `BEGIN` control does instead of restating the rule —
/// a second statement of it is exactly how blocker 14 shipped.
#[cfg(test)]
pub(crate) use broker::invocation_fence as broker_invocation_fence;
#[allow(unused_imports)] // constructed by the pending workspace broker composition
pub(crate) use broker::{CgroupLauncherClient, ProcessHandle, UnixBrokerLauncher, birth_authority};

#[derive(Clone, Debug, PartialEq, Eq)]
struct UnresolvedInvocation {
    identity: TaskInvocationLeaseIdentity,
    pod_uid: String,
    fence: Option<LeaseFencingToken>,
    terminal_intent: bool,
    watchdog_notified: bool,
    recorded_at_ms: u128,
}

/// Re-wrap an `io::Error` from journal setup so the message carries the failing
/// operation, the exact directory and the OS errno. The kind is preserved so
/// callers that match on `ErrorKind` are unaffected.
fn named_journal_error(operation: &str, directory: &Path, error: io::Error) -> io::Error {
    let errno = error
        .raw_os_error()
        .map_or_else(|| "none".to_string(), |code| code.to_string());
    io::Error::new(
        error.kind(),
        format!(
            "invocation journal {operation} on {} failed (errno {errno}): {error}",
            directory.display()
        ),
    )
}

/// Pod-local write-ahead journal. Replacements fsync the file and parent.
pub struct InvocationJournal {
    directory: PathBuf,
    pod_uid: String,
    update_lock: Mutex<()>,
}
impl InvocationJournal {
    /// Open (creating if needed) the pod-local journal directory.
    ///
    /// # Every failure here names its operation, path and errno
    ///
    /// This constructor runs on the worker's REQUIRED startup path, before a
    /// session exists, and a failure aborts the whole task-run Pod. It used to
    /// propagate the bare `io::Error`, so an armed production rollout died with
    /// nothing but `Read-only file system (os error 30)` in the log — no
    /// operation, no directory — and cost a deploy cycle to attribute. (The
    /// cause: the compiled-in default directory sits inside the read-only
    /// `spec` Secret mount; the render now supplies a writable volume, see
    /// `djinn_k8s::invocation_journal`.) Naming the operation and the path is
    /// what makes the next such failure readable from one log line.
    pub fn new(directory: PathBuf, pod_uid: String) -> io::Result<Self> {
        if pod_uid.is_empty() || pod_uid.contains('/') || pod_uid.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid pod UID",
            ));
        }
        fs::create_dir_all(&directory)
            .map_err(|error| named_journal_error("create_dir_all", &directory, error))?;
        File::open(&directory)
            .and_then(|handle| handle.sync_all())
            .map_err(|error| named_journal_error("open+fsync", &directory, error))?;
        Ok(Self {
            directory,
            pod_uid,
            update_lock: Mutex::new(()),
        })
    }
    fn record_at(
        &self,
        identity: &TaskInvocationLeaseIdentity,
        fence: Option<LeaseFencingToken>,
        terminal_intent: bool,
        recorded_at: SystemTime,
    ) -> io::Result<()> {
        let _guard = self.update_lock.lock().unwrap();
        let watchdog_notified = match fs::read_to_string(self.path(identity)) {
            Ok(content) => {
                let current = parse_unresolved(&content)?;
                current.identity == *identity && current.watchdog_notified
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        self.replace_unlocked(UnresolvedInvocation {
            identity: identity.clone(),
            pod_uid: self.pod_uid.clone(),
            fence,
            terminal_intent,
            watchdog_notified,
            recorded_at_ms: recorded_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        })
    }
    /// Serialized compare-and-set: persist `watchdog_notified` for the current
    /// durable record, then hand back the exact-pod termination request built
    /// from that record. The immutable pod UID (and task/task-run identity) come
    /// from the durable content, never from the reconstructing process's own
    /// environment, so recovery targets recorded pod A even when this process is
    /// pod B. Returns `None` when the record is gone, has advanced to a
    /// different identity, or was already notified — the persisted bit is what
    /// makes the callback fire at most once across recurring scans.
    fn notify_if_current(
        &self,
        identity: &TaskInvocationLeaseIdentity,
    ) -> io::Result<Option<WatchdogTerminationRequest>> {
        let _guard = self.update_lock.lock().unwrap();
        let content = match fs::read_to_string(self.path(identity)) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut current = parse_unresolved(&content)?;
        if current.identity != *identity || current.watchdog_notified {
            return Ok(None);
        }
        current.watchdog_notified = true;
        let request = WatchdogTerminationRequest {
            task_id: current.identity.task_id.clone(),
            task_run_id: current.identity.task_run_id.clone(),
            pod_uid: current.pod_uid.clone(),
        };
        self.replace_unlocked(current)?;
        Ok(Some(request))
    }
    fn clear(&self, identity: &TaskInvocationLeaseIdentity) -> io::Result<()> {
        let _guard = self.update_lock.lock().unwrap();
        match fs::remove_file(self.path(identity)) {
            Ok(()) => File::open(&self.directory)?.sync_all(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
    fn path(&self, identity: &TaskInvocationLeaseIdentity) -> PathBuf {
        self.directory
            .join(format!("{}.invocation", identity.invocation_id))
    }
    fn replace_unlocked(&self, record: UnresolvedInvocation) -> io::Result<()> {
        let path = self.path(&record.identity);
        let tmp = path.with_extension("tmp");
        let content = format!(
            "task_id={}\ntask_run_id={}\ninvocation_id={}\npod_uid={}\nfence={}\nterminal_intent={}\nwatchdog_notified={}\nrecorded_at_ms={}\n",
            record.identity.task_id,
            record.identity.task_run_id,
            record.identity.invocation_id,
            record.pod_uid,
            record.fence.map(|f| f.0.to_string()).unwrap_or_default(),
            record.terminal_intent,
            record.watchdog_notified,
            record.recorded_at_ms
        );
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(tmp, path)?;
        File::open(&self.directory)?.sync_all()
    }
    fn unresolved(&self) -> io::Result<Vec<UnresolvedInvocation>> {
        let _guard = self.update_lock.lock().unwrap();
        fs::read_dir(&self.directory)?
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().and_then(|v| v.to_str()) == Some("invocation"))
            .map(|e| parse_unresolved(&fs::read_to_string(e.path())?))
            .collect()
    }
}
fn parse_unresolved(content: &str) -> io::Result<UnresolvedInvocation> {
    let value = |key: &str| {
        content
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, format!("journal missing {key}"))
            })
    };
    let fence = content
        .lines()
        .find_map(|line| line.strip_prefix("fence="))
        .filter(|v| !v.is_empty())
        .map(|v| v.parse::<u64>().map(LeaseFencingToken))
        .transpose()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid journal fence"))?;
    let terminal_intent = content
        .lines()
        .find_map(|line| line.strip_prefix("terminal_intent="))
        .map(|v| v == "true")
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "journal missing terminal intent",
            )
        })?;
    Ok(UnresolvedInvocation {
        identity: TaskInvocationLeaseIdentity {
            task_id: value("task_id")?,
            task_run_id: value("task_run_id")?,
            invocation_id: value("invocation_id")?,
        },
        pod_uid: value("pod_uid")?,
        fence,
        terminal_intent,
        watchdog_notified: content
            .lines()
            .find_map(|line| line.strip_prefix("watchdog_notified="))
            .map(|v| v == "true")
            .unwrap_or(false),
        recorded_at_ms: content
            .lines()
            .find_map(|line| line.strip_prefix("recorded_at_ms="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    })
}

/// Explicit recovery seams keep restart grace and watchdog delivery testable.
pub struct InvocationRecovery<'a> {
    pub journal: &'a InvocationJournal,
    pub services: &'a dyn SupervisorServices,
    pub clock: &'a dyn Clock,
    pub watchdog_grace: Duration,
}

impl<'a> InvocationRecovery<'a> {
    /// Sweep the durable journal, reconciling terminal records and firing the
    /// exact-pod `watchdog` for a grace-expired record exactly once. The
    /// callback receives the full [`WatchdogTerminationRequest`] read from the
    /// durable record (task, task-run, immutable pod UID); it is invoked only
    /// after `watchdog_notified` is durably persisted. A callback that cannot
    /// confirm termination (lost/unavailable transport) must leave the record
    /// in place — this loop never re-issues the callback once the bit is set,
    /// and only a matching durable terminal confirmation clears the record.
    pub async fn run<W, Fut>(&self, mut watchdog: W) -> io::Result<()>
    where
        W: FnMut(WatchdogTerminationRequest) -> Fut,
        Fut: Future<Output = ()>,
    {
        for record in self.journal.unresolved()? {
            let lease = LeaseIdentity::TaskInvocation(record.identity.clone());
            // Always query the coordinator's durable view. An unavailable reply
            // is ambiguous and never proves that local descendants are gone.
            let status = self
                .services
                .lease_status(LeaseStatusRequest {
                    identity: lease.clone(),
                })
                .await;
            // A record is evidence of an unresolved pod until both sides have
            // made durable terminal progress. Never turn unavailable,
            // conflicting, mismatched, or active status into a cleanup request.
            let confirmed = record.terminal_intent
                && (terminal_status_matches(&status, record.fence.as_ref())
                    || matches!(
                        status,
                        LeaseResult::Status(LeaseStatus {
                            state: LeaseState::Queued,
                            fencing_token: None,
                            ..
                        }) if record.fence.is_none()
                    ) && reconcile_recovered_queued_lease(self.services, lease).await);
            if confirmed {
                self.journal.clear(&record.identity)?;
                continue;
            }
            let recorded_at = UNIX_EPOCH + Duration::from_millis(record.recorded_at_ms as u64);
            if self
                .clock
                .now()
                .duration_since(recorded_at)
                .unwrap_or_default()
                >= self.watchdog_grace
                && !record.watchdog_notified
            {
                // Re-read while serialized with lifecycle writes. Only the
                // current durable record can authorize exact-pod deletion, and
                // it carries the recorded task/task-run/pod UID the callback
                // must target — not this process's current identity.
                if let Some(request) = self.journal.notify_if_current(&record.identity)? {
                    watchdog(request).await;
                }
            }
        }
        Ok(())
    }
}
#[allow(dead_code)]
pub async fn recover_unresolved_invocations<W, Fut>(
    journal: &InvocationJournal,
    services: &dyn SupervisorServices,
    watchdog: W,
) -> io::Result<()>
where
    W: FnMut(WatchdogTerminationRequest) -> Fut,
    Fut: Future<Output = ()>,
{
    let clock = SystemClock::new();
    InvocationRecovery {
        journal,
        services,
        clock: &clock,
        watchdog_grace: Duration::ZERO,
    }
    .run(watchdog)
    .await
}

#[derive(Clone, Debug)]
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
pub(crate) struct LeaseInvocationConfig {
    pub task_id: String,
    pub task_run_id: String,
    pub pod_uid: String,
    pub cpu_usage_threshold_usec: u64,
    /// How long this invocation may sit in the durable queue before the
    /// coordinator terminalizes it as `deadline_expired`.
    ///
    /// Deliberately a [`Duration`] and not a bare `i64`: the wire contract
    /// ([`LeaseDeadlines`]) carries ABSOLUTE epoch milliseconds, and a caller
    /// that wrote its intended timeout there produced a 1970 deadline that
    /// expired every task-invocation lease on arrival. The conversion to an
    /// absolute instant happens once, in [`LeaseInvocationRunner::lease_deadlines`],
    /// against this runner's clock at the moment the queue request is issued —
    /// so a config value can no longer be mistaken for a timestamp.
    pub queue_timeout: Duration,
    /// How long a granted lease may remain unlaunched before the coordinator
    /// marks it `suspect`. Same units contract as [`Self::queue_timeout`].
    pub launch_timeout: Duration,
    pub timeout: Duration,
}

/// A lease response that stops the invocation.
///
/// There is deliberately no wait-timeout variant: losing the queue is
/// contention, not a failure, and contention degrades to unleased execution
/// (see [`lease_failure`]) instead of killing the command. Only responses that
/// mean the lease authority cannot be used coherently at all — an identity
/// conflict, or repeated unavailability — end the invocation here.
#[derive(Debug)]
#[allow(dead_code)] // consumed by this runner now and workspace lease wiring next
pub(crate) enum LeaseInvocationError {
    Process(ProcessRunError),
    Launcher(io::Error),
    LeaseIdentityConflict,
    LeaseUnavailable,
}

#[derive(Debug)]
#[allow(dead_code)] // consumed by this runner now and workspace lease wiring next
pub(crate) struct LeaseInvocationOutput {
    pub(crate) process: ProcessOutput,
    pub(crate) identity: TaskInvocationLeaseIdentity,
}

/// Launcher-backed invocation state machine. A queue is issued only after a
/// measured CPU threshold; terminal intent is set before cgroup kill/reconcile.
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
pub(crate) struct LeaseInvocationRunner {
    services: Arc<dyn SupervisorServices>,
    /// The durable admission authority this runner asks before every launch.
    ///
    /// Separate from `services` on purpose. This used to be a defaulted
    /// `SupervisorServices` method, and because the production composition hands
    /// this runner the worker's `RpcServices` — which never overrode it — every
    /// invocation took the fail-closed default while the epoch was fully armed
    /// (goxi launcher blocker 13). A mandatory constructor parameter cannot be
    /// silently not-implemented.
    lift: Arc<dyn InvocationLiftAuthority>,
    launcher: Arc<dyn CgroupLauncherClient>,
    clock: Arc<dyn Clock>,
    journal: Option<Arc<InvocationJournal>>,
}
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
impl LeaseInvocationRunner {
    pub(crate) fn new(
        services: Arc<dyn SupervisorServices>,
        lift: Arc<dyn InvocationLiftAuthority>,
        launcher: Arc<dyn CgroupLauncherClient>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            services,
            lift,
            launcher,
            clock,
            journal: None,
        }
    }
    pub(crate) fn with_journal(mut self, journal: Arc<InvocationJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Render the invocation's configured timeouts as the wire deadlines the
    /// coordinator actually understands.
    ///
    /// [`LeaseDeadlines`] carries ABSOLUTE Unix epoch milliseconds: the
    /// coordinator persists them as timestamps and `expire_deadlines` compares
    /// them against its own wall clock. Handing it a raw timeout instead placed
    /// every task-invocation deadline in 1970, so the durable row was
    /// terminalized as `deadline_expired` before the queue could ever be
    /// granted and the invocation degraded to the launcher's unleased 250m
    /// quota for the whole command. The conversion therefore lives here — one
    /// place, at the moment the request is issued, reading this runner's
    /// injected clock so it stays deterministic under test.
    fn lease_deadlines(&self, config: &LeaseInvocationConfig) -> LeaseDeadlines {
        let now_ms = epoch_ms(self.clock.now());
        LeaseDeadlines {
            queue_deadline_ms: deadline_epoch_ms(now_ms, config.queue_timeout),
            launch_deadline_ms: deadline_epoch_ms(now_ms, config.launch_timeout),
        }
    }

    pub(crate) async fn output(
        &self,
        cmd: Command,
        config: LeaseInvocationConfig,
        cancel: CancellationToken,
    ) -> Result<LeaseInvocationOutput, LeaseInvocationError> {
        let identity = TaskInvocationLeaseIdentity {
            task_id: config.task_id.clone(),
            task_run_id: config.task_run_id.clone(),
            invocation_id: uuid::Uuid::now_v7().to_string(),
        };
        let lease = LeaseIdentity::TaskInvocation(identity.clone());
        // NOT journalled here. The journal exists to recover a lease that this
        // pod may have created at the coordinator; an invocation that never
        // escalates never calls `queue_lease`, so it owns no durable coordinator
        // state and there is nothing to recover. Writing here anyway made every
        // cheap command (`git status`, a grep) leave a record that the terminal
        // path below could not clear — the clear is gated on `queued` — and the
        // pod-local recovery sweep then read that permanently-unresolved record
        // as an orphan and fired the exact-pod watchdog against its OWN pod.
        // With a 300s `WATCHDOG_GRACE` and a 300s sweep tick, every worker pod
        // deleted itself ~600s after start, mid-session, and its task bounced
        // `in_progress -> open` forever. The write-ahead record is now taken at
        // the escalation point below, immediately before the `queue_lease` RPC.
        // Resolve the durable admission decision ONCE, before the leaf exists.
        //
        // This read used to happen only after a successful bind, which made it
        // unable to influence the one thing it actually governs: the quota the
        // leaf is born at. `Unleased` was then implemented as `{}` — and because
        // the leaf had already been pinned to the 250m unleased quota, that
        // "no-op" clamped every command for its whole life. Production ran with
        // the `admission_handoff` row ABSENT, so `Unleased` was the decision for
        // EVERY invocation: a measured leaf reached 21.1 CPU-seconds (84x the
        // 0.25 CPU-s escalation threshold) while `cpu.max` never left
        // `25000 100000`. Reading it here, and deriving the birth authority from
        // it, is what makes an unarmed authority mean "do not clamp" instead of
        // "clamp forever".
        //
        // One read per invocation is also the coherent choice: the birth quota is
        // committed from this value and cannot be revised, so a mid-invocation
        // epoch change must not be acted on by the bind arm below.
        //
        // Asked of the injected [`InvocationLiftAuthority`], NOT of
        // `self.services`. The lift decision was a defaulted `SupervisorServices`
        // method until goxi blocker 13: the pod's runner is composed around
        // `RpcServices`, which never overrode it, so this line silently returned
        // `Unleased` for every invocation against a fully armed epoch
        // (`ForwardOverlap` · epoch 3 · v1 `Enforce` · both acks at 3) and every
        // leaf was born at `cpu.max=[max 100000]` and never transitioned.
        let lift_decision = self.lift.invocation_lift_decision().await;
        let authority = birth_authority(lift_decision);
        let mut child = self
            .launcher
            .launch(cmd, &identity, authority)
            .map_err(LeaseInvocationError::Launcher)?;
        // The path reported nothing at all across four armed production
        // rollouts: the entire diagnosis had to be done by reading cgroupfs by
        // hand on the node. One line per invocation, naming the decision and the
        // quota actually committed, is the difference between "the escalation
        // never fired" and knowing why.
        tracing::info!(
            invocation_id = %identity.invocation_id,
            task_run_id = %identity.task_run_id,
            decision = ?lift_decision,
            authority = ?authority,
            threshold_usec = config.cpu_usage_threshold_usec,
            "lease invocation launched into a cgroup leaf"
        );
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let mut deadline = self.clock.now_instant() + config.timeout;
        let (mut queued, mut fence, mut credit_used, mut lease_error) = (false, None, false, None);
        let mut unavailable_responses = 0_u8;
        // Set when this invocation can no longer be escalated: either the
        // durable queue ended without capacity for it (`lease_failure`), or the
        // privileged launcher broker REFUSED the one-way cgroup lift. From then
        // on the child runs to its own terminal state at the launcher's unleased
        // quota and the lease authority is never contacted again for a grant.
        // See `lease_failure` for why the degrade is one-way.
        //
        // Both causes share one rule: contention or a lease-subsystem defect
        // makes a command SLOW, never dead. Nothing here kills the child, and
        // the durable record is still reconciled to terminal after the loop.
        let mut unleased_degrade = false;
        // The absolute queue deadline this invocation sent, once it queued.
        // `0` means it never reached the lease authority.
        let mut queue_deadline_ms = 0_i64;
        // This variable is assigned only by `terminal_now` or by a service
        // wait that observed terminal intent. Consequently no later response
        // can authorize a cgroup lift.
        let (observed, termination) = 'invocation: loop {
            drain_remote(&mut *child, &mut stdout, &mut stderr)?;
            if let Some(terminal) =
                terminal_now(&mut *child, &cancel, self.clock.as_ref(), deadline)?
            {
                break terminal;
            }
            let cpu = child.sample_cpu().map_err(LeaseInvocationError::Launcher)?;
            let result = if unleased_degrade {
                // Degraded: the child keeps running at the launcher's unleased
                // quota (250m) until it exits on its own, its command timeout
                // fires, or it is cancelled. Contention makes a command slow,
                // never dead — nothing here kills the child, and no further
                // lease request is issued. The durable record is still
                // reconciled to terminal after the loop.
                tokio::task::yield_now().await;
                continue;
            } else if !queued && cpu.usage_usec >= config.cpu_usage_threshold_usec {
                queued = true;
                // Captured HERE, not hoisted out of the loop: the deadline is
                // relative to the moment this invocation actually crossed the
                // escalation threshold, which for a long command can be minutes
                // after launch. Retained so the degrade diagnostic can say
                // whether the queue position had genuinely expired — a status
                // read reports a terminalized row as `Cancelled` and carries no
                // terminal reason over the wire, so the deadline and the clock
                // are the only evidence available in the pod.
                let queued_deadlines = self.lease_deadlines(&config);
                queue_deadline_ms = queued_deadlines.queue_deadline_ms;
                // Write-ahead: the record must be durable BEFORE the request
                // goes out, so a pod that dies mid-`queue_lease` still leaves
                // evidence a successor can reconcile. This is the first point
                // at which durable coordinator state can exist for this
                // invocation, and therefore the earliest point at which the
                // record is anything but garbage.
                if let Some(journal) = &self.journal {
                    journal
                        .record_at(&identity, None, false, self.clock.now())
                        .map_err(LeaseInvocationError::Launcher)?;
                }
                match await_lease_or_terminal(
                    self.services.queue_lease(LeaseQueueRequest {
                        identity: lease.clone(),
                        deadlines: queued_deadlines,
                    }),
                    &mut *child,
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
                    &mut *child,
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
            // An unavailable response is transport-ambiguous: the idempotent
            // request may have reached the coordinator. Re-read status before
            // turning repeated unavailability into the typed terminal result.
            if matches!(result, LeaseResult::LeaseUnavailable) {
                unavailable_responses += 1;
                if unavailable_responses >= 3 {
                    lease_failure(
                        DegradeDiagnostic {
                            identity: &identity,
                            observed_usage_usec: cpu.usage_usec,
                            authority,
                            queue_deadline_ms,
                            now_ms: epoch_ms(self.clock.now()),
                        },
                        LeaseResult::LeaseUnavailable,
                        &mut deadline,
                        &mut credit_used,
                        &mut lease_error,
                        &mut unleased_degrade,
                    );
                    break (
                        child.try_wait().map_err(started_lease_error)?,
                        ProcessTermination::Cancelled,
                    );
                }
                tokio::task::yield_now().await;
                continue;
            }
            unavailable_responses = 0;
            // Once a fence has been recorded, status polling remains useful
            // for terminal reconciliation but can never re-enter grant/lift.
            let grant = if fence.is_none() {
                match result {
                    LeaseResult::Granted(grant) => Some(grant.fencing_token),
                    LeaseResult::Status(status)
                        if matches!(
                            status.state,
                            LeaseState::Granted
                                | LeaseState::Launching
                                | LeaseState::Bound
                                | LeaseState::Active
                        ) =>
                    {
                        status.fencing_token
                    }
                    other => {
                        lease_failure(
                            DegradeDiagnostic {
                                identity: &identity,
                                observed_usage_usec: cpu.usage_usec,
                                authority,
                                queue_deadline_ms,
                                now_ms: epoch_ms(self.clock.now()),
                            },
                            other,
                            &mut deadline,
                            &mut credit_used,
                            &mut lease_error,
                            &mut unleased_degrade,
                        );
                        None
                    }
                }
            } else {
                None
            };
            if let Some(token) = grant {
                let durable = match await_lease_or_terminal(
                    self.services.grant_lease(LeaseGrantRequest {
                        identity: lease.clone(),
                        fencing_token: token.clone(),
                    }),
                    &mut *child,
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
                            terminal_now(&mut *child, &cancel, self.clock.as_ref(), deadline)?
                        {
                            break 'invocation terminal;
                        }
                        match self
                            .services
                            .bind_lease_pod(LeaseBindRequest {
                                identity: lease.clone(),
                                fencing_token: token.clone(),
                                pod_uid: config.pod_uid.clone(),
                            })
                            .await
                        {
                            LeaseResult::Bound(status)
                                if status.fencing_token.as_ref() == Some(&token)
                                    && status.pod_uid.as_deref()
                                        == Some(config.pod_uid.as_str()) =>
                            {
                                // A matching durable bind is necessary but not
                                // sufficient: the durable admission epoch decides
                                // whether the launcher may lift the reserved
                                // quota. Only a committed overlap /
                                // invocation-primary epoch (v1 enforcing) lifts;
                                // shadow observes without lifting; every other
                                // epoch (baseline, missing, unreadable, stale)
                                // never lifts — and, because it can never lift,
                                // the leaf it governs was already born WITHOUT a
                                // quota, so there is nothing here to raise. The
                                // durable lease is still held and reconciled to
                                // terminal below in all three cases, so `fence` is
                                // always recorded.
                                // The decision resolved before launch, NOT a
                                // fresh read: the birth quota was already
                                // committed from it, so a re-read could ask for a
                                // lift on a leaf the broker created unarmed (the
                                // launcher rejects that with
                                // `LiftWithoutAuthority`).
                                match lift_decision {
                                    InvocationLiftDecision::Lift => {
                                        // The validated fence must survive before
                                        // the irreversible cgroup lift. A failed
                                        // journal write therefore prevents the
                                        // lift entirely.
                                        if let Some(journal) = &self.journal {
                                            journal
                                                .record_at(
                                                    &identity,
                                                    Some(token.clone()),
                                                    false,
                                                    self.clock.now(),
                                                )
                                                .map_err(LeaseInvocationError::Launcher)?;
                                        }
                                        // A REFUSED LIFT DEGRADES; IT DOES NOT
                                        // FAIL THE COMMAND.
                                        //
                                        // This used to be `?`, so any refusal
                                        // from the privileged broker became
                                        // `LeaseInvocationError::Launcher` and
                                        // failed the whole shell tool call:
                                        //
                                        //   ReplyLoop: tool call returned error
                                        //   tool=shell error=failed to run shell
                                        //   command: lease invocation failed: …
                                        //
                                        // measured in production at 5 failures
                                        // per 10 launches. That is the worst
                                        // available coupling: a defect in a
                                        // *throttling optimisation* took out the
                                        // agent's ability to run commands at
                                        // all, and the fallback it denied itself
                                        // — keep running clamped — is strictly
                                        // better than dying.
                                        //
                                        // It also contradicted the precedent
                                        // this very runner already sets one arm
                                        // up: a lost lease QUEUE degrades to
                                        // continued unleased execution
                                        // (`lease_failure` → `unleased_degrade`)
                                        // precisely because contention must make
                                        // a command slow, never dead. A rejected
                                        // lift is the same class of event and is
                                        // now handled the same way.
                                        //
                                        // Nothing is swallowed. The child keeps
                                        // running at the 250m unleased quota it
                                        // was born at (containment is
                                        // unaffected: the leaf, its kill path
                                        // and its cpu.stat are all intact), the
                                        // failure is logged at ERROR with the
                                        // broker's refusal category, a telemetry
                                        // counter records it, and the durable
                                        // lease is still reconciled to terminal
                                        // because `fence` is recorded below.
                                        match child.fenced_lift() {
                                            Ok(()) => tracing::info!(
                                                invocation_id = %identity.invocation_id,
                                                observed_usage_usec = cpu.usage_usec,
                                                "lease invocation escalated: cgroup quota lifted"
                                            ),
                                            Err(error) => {
                                                djinn_telemetry::build_admission::record_lift_rejected();
                                                tracing::error!(
                                                    invocation_id = %identity.invocation_id,
                                                    task_run_id = %identity.task_run_id,
                                                    observed_usage_usec = cpu.usage_usec,
                                                    error = %error,
                                                    "lease invocation lift REFUSED by the launcher \
                                                     broker; the command keeps running CLAMPED at \
                                                     the unleased quota instead of failing. This \
                                                     is a lease-subsystem defect: the invocation \
                                                     held a matching durable grant and the epoch \
                                                     authorized a lift"
                                                );
                                                // One-way, exactly like the lost
                                                // queue: do not re-enter the
                                                // grant/lift path for an
                                                // invocation whose one-way lift
                                                // has already been refused.
                                                unleased_degrade = true;
                                            }
                                        }
                                    }
                                    InvocationLiftDecision::Shadow => {
                                        // Reaching a valid bind means v1 would
                                        // have escalated (lifted); record the
                                        // bounded shadow observation but leave
                                        // cpu.max throttled under v0.
                                        //
                                        // Shadow CLAMPS. The leaf stays at the
                                        // broker's 250m unleased quota for the
                                        // whole command — arming shadow makes
                                        // leased builds slower, never faster. It
                                        // is an observation mode; see
                                        // `evaluate_invocation_lift`. No
                                        // irreversible lift occurs, so no
                                        // fence-before-lift journal write is
                                        // required; `fence` is still reconciled
                                        // to terminal below. The complementary
                                        // `would_throttle` arm is recorded after
                                        // the loop for invocations that never
                                        // crossed the escalation threshold.
                                        djinn_telemetry::build_admission::record_shadow_invocation(
                                            true,
                                        );
                                    }
                                    InvocationLiftDecision::Unleased => {}
                                }
                                fence = Some(token);
                            }
                            other => lease_failure(
                                DegradeDiagnostic {
                                    identity: &identity,
                                    observed_usage_usec: cpu.usage_usec,
                                    authority,
                                    queue_deadline_ms,
                                    now_ms: epoch_ms(self.clock.now()),
                                },
                                other,
                                &mut deadline,
                                &mut credit_used,
                                &mut lease_error,
                                &mut unleased_degrade,
                            ),
                        }
                    }
                    LeaseResult::LeaseUnavailable => {
                        unavailable_responses += 1;
                        if unavailable_responses >= 3 {
                            lease_failure(
                                DegradeDiagnostic {
                                    identity: &identity,
                                    observed_usage_usec: cpu.usage_usec,
                                    authority,
                                    queue_deadline_ms,
                                    now_ms: epoch_ms(self.clock.now()),
                                },
                                LeaseResult::LeaseUnavailable,
                                &mut deadline,
                                &mut credit_used,
                                &mut lease_error,
                                &mut unleased_degrade,
                            );
                        }
                    }
                    other => lease_failure(
                        DegradeDiagnostic {
                            identity: &identity,
                            observed_usage_usec: cpu.usage_usec,
                            authority,
                            queue_deadline_ms,
                            now_ms: epoch_ms(self.clock.now()),
                        },
                        other,
                        &mut deadline,
                        &mut credit_used,
                        &mut lease_error,
                        &mut unleased_degrade,
                    ),
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
        // Gated on `queued` for the same reason as the write-ahead record: a
        // non-escalated invocation has no record to advance to terminal, and an
        // unconditional write here would simply re-create the leaked record the
        // clear below cannot remove.
        if queued && let Some(journal) = &self.journal {
            journal
                .record_at(&identity, fence.clone(), true, self.clock.now())
                .map_err(LeaseInvocationError::Launcher)?;
        }
        child.kill().map_err(LeaseInvocationError::Launcher)?;
        child.wait_empty().map_err(LeaseInvocationError::Launcher)?;
        drain_remote(&mut *child, &mut stdout, &mut stderr)?;
        let status = observed.unwrap_or(child.wait().map_err(started_lease_error)?);
        let process = ProcessOutput {
            output: Output {
                status,
                stdout,
                stderr,
            },
            termination,
        };
        child.cleanup().map_err(LeaseInvocationError::Launcher)?;
        if queued
            && reconcile_terminal_lease(self.services.as_ref(), lease, fence).await
            && let Some(journal) = &self.journal
        {
            journal
                .clear(&identity)
                .map_err(LeaseInvocationError::Launcher)?;
        }
        // The shadow measurement baseline, and the complement of the
        // `would_escalate` arm recorded at the bind above: this invocation ran
        // to terminal without ever crossing `cpu_usage_threshold_usec`
        // (`queued` is false), so it was never escalated to the lease authority
        // and v1 would have left it throttled. Recording only the escalating
        // arm would make the ratio that decides cutover safety ("of all
        // observed invocations, what fraction would v1 have escalated?")
        // unanswerable. The two arms are mutually exclusive: escalation
        // requires a grant, which requires `queued`.
        //
        // Observation only. The epoch is read once, after the child is terminal
        // and the durable lease is reconciled, and a read failure projects to
        // `Unleased` — nothing here can lift cpu.max, mint a fence, or move
        // lease state.
        // The epoch is the one resolved before launch, for the same reason the
        // bind arm above uses it: this invocation's whole life was governed by
        // that decision, so the shadow denominator must describe the same epoch
        // the numerator does.
        if !queued && matches!(lift_decision, InvocationLiftDecision::Shadow) {
            djinn_telemetry::build_admission::record_shadow_invocation(false);
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
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
enum LeaseWait {
    Response(LeaseResult),
    Terminal((Option<std::process::ExitStatus>, ProcessTermination)),
}

#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
async fn await_lease_or_terminal<F>(
    request: F,
    child: &mut dyn ProcessHandle,
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

#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
fn terminal_now(
    child: &mut dyn ProcessHandle,
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

fn drain_remote(
    child: &mut dyn ProcessHandle,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<(), LeaseInvocationError> {
    stdout.extend(
        child
            .drain_stdout()
            .map_err(LeaseInvocationError::Launcher)?,
    );
    stderr.extend(
        child
            .drain_stderr()
            .map_err(LeaseInvocationError::Launcher)?,
    );
    Ok(())
}

/// A recovery clear requires a terminal coordinator observation for this
/// record's durable fence. Terminal operation acknowledgements alone are not
/// enough: their reply can have been lost or belong to an ambiguous retry.
fn terminal_status_matches(result: &LeaseResult, fence: Option<&LeaseFencingToken>) -> bool {
    match result {
        LeaseResult::Status(LeaseStatus {
            state: LeaseState::Cancelled | LeaseState::Released,
            fencing_token,
            ..
        }) => fencing_token.as_ref() == fence,
        LeaseResult::Cancelled { .. }
        | LeaseResult::Released { .. }
        | LeaseResult::Abandoned { .. } => true,
        _ => false,
    }
}

/// Only an unfenced queued record is safe to abandon after restart. A durable
/// status query following the request, rather than the abandon acknowledgement,
/// confirms terminal cleanup before the journal can be removed.
async fn reconcile_recovered_queued_lease(
    services: &dyn SupervisorServices,
    lease: LeaseIdentity,
) -> bool {
    let _ = services
        .abandon_lease(LeaseAbandonRequest {
            identity: lease.clone(),
            candidate_cleanup: false,
        })
        .await;
    terminal_status_matches(
        &services
            .lease_status(LeaseStatusRequest { identity: lease })
            .await,
        None,
    )
}

/// Re-read durable state after every uncertain cleanup response. Operations
/// are idempotent and reuse the same fence, so a lost request cannot cause a
/// second capacity return or leave a known grant unreleased.
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
async fn reconcile_terminal_lease(
    services: &dyn SupervisorServices,
    lease: LeaseIdentity,
    observed_fence: Option<LeaseFencingToken>,
) -> bool {
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
                return true;
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
            | LeaseResult::Abandoned { .. } => return true,
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
            return true;
        }
        tokio::task::yield_now().await;
    }
    false
}

#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
fn started_lease_error(error: io::Error) -> LeaseInvocationError {
    LeaseInvocationError::Process(ProcessRunError::Started(error))
}

/// Wall-clock instant as Unix epoch milliseconds. A pre-epoch clock reads as 0,
/// which the deadline contract treats as "no deadline" rather than "expired".
fn epoch_ms(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
    })
}

/// Project a relative timeout onto the absolute epoch-millisecond instant the
/// coordinator compares against its own clock.
///
/// `Duration::ZERO` renders `0`, which [`LeaseDeadlines`] defines as *no
/// deadline* (the durable column stays NULL) — the same thing the graph-warm
/// recovery and worker paths already pass. It is deliberately not "expires
/// immediately".
fn deadline_epoch_ms(now_ms: i64, timeout: Duration) -> i64 {
    if timeout.is_zero() {
        return 0;
    }
    now_ms.saturating_add(i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX))
}
/// Classify a lease response that did not produce a usable grant.
///
/// Contention is not a failure. A command that cannot get a lease must queue
/// and be slow, not die: when the durable queue ends without capacity — a wait
/// timeout, or a terminal record observed before any grant — the invocation
/// sets `unleased` and the caller keeps the child running at the launcher's
/// unleased quota. Killing the child there was the old behaviour and it burned
/// the agent's session on `LeaseWaitTimeout` for work that was merely queued.
///
/// The degrade is **one-way**. Every terminal reason, `deadline_expired`
/// included, is a terminal durable state for this invocation id; the
/// coordinator can never grant that row again, and this invocation's identity
/// is immutable (it is journaled before launch and drives watchdog recovery),
/// so re-escalating would require minting a second lease identity for one
/// child. Re-escalation is therefore out of reach without a redesign of the
/// identity/journal contract.
///
/// Only responses meaning the lease authority cannot be used coherently at all
/// — an identity conflict, or repeated unavailability — remain hard errors.
///
/// # This path REPORTS
///
/// The degrade used to be completely silent: no `tracing::` call anywhere on
/// it, so a command that lost its queue was indistinguishable in the logs from
/// one that never needed a lease. The only observable was a `lease invocation
/// launched` line with no matching `cgroup quota lifted`, which is an absence —
/// nothing greps for it and no alert fires on it. Four armed rollouts were
/// diagnosed by reading `cpu.max` out of cgroupfs on the node by hand, and the
/// 16x fleet-wide slowdown this function's caller now names ran for days.
/// [`degrade_reason`] and the `warn!` below are that missing event.
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
fn lease_failure(
    diagnostic: DegradeDiagnostic<'_>,
    result: LeaseResult,
    deadline: &mut std::time::Instant,
    credit_used: &mut bool,
    output: &mut Option<LeaseInvocationError>,
    unleased: &mut bool,
) {
    // Read before the match consumes `result`, and compared against the state
    // BEFORE it so the one-way degrade reports exactly once per invocation
    // rather than on every subsequent terminal status poll.
    let reason = degrade_reason(&result);
    let already_degraded = *unleased;
    lease_failure_classify(result, deadline, credit_used, output, unleased);
    if *unleased && !already_degraded {
        djinn_telemetry::build_admission::record_invocation_degraded(reason);
        tracing::warn!(
            invocation_id = %diagnostic.identity.invocation_id,
            task_run_id = %diagnostic.identity.task_run_id,
            terminal_reason = reason,
            observed_usage_usec = diagnostic.observed_usage_usec,
            authority = ?diagnostic.authority,
            degraded_quota = degraded_quota(diagnostic.authority),
            queue_deadline_ms = diagnostic.queue_deadline_ms,
            now_ms = diagnostic.now_ms,
            queue_deadline_passed =
                diagnostic.queue_deadline_ms > 0 && diagnostic.now_ms >= diagnostic.queue_deadline_ms,
            "lease invocation DEGRADED: the build lease will never be granted, so the command \
             runs the rest of its life at the quota it was born at. Under an Armed authority that \
             is the launcher's unleased quota (250m by default) — a ~16x slowdown for a compile — \
             and the degrade is one-way: this invocation will not be escalated again"
        );
    }
}

/// The identity and measurements the degrade diagnostic names.
///
/// Carried into [`lease_failure`] rather than logged at its five call sites so
/// no future arm can be added that degrades without reporting.
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
struct DegradeDiagnostic<'a> {
    identity: &'a TaskInvocationLeaseIdentity,
    /// `cpu.stat usage_usec` for this leaf at the moment it lost the lease.
    /// This is the number that makes a degrade actionable: 52.8 CPU-seconds
    /// against a 0.25 CPU-s escalation threshold says the throttling is being
    /// applied to real build work, not to a trivial command.
    observed_usage_usec: u64,
    /// The authority the leaf was BORN at. `Armed` means it is clamped for the
    /// rest of its life; `Unarmed` means there was never a quota to lift and
    /// the degrade costs nothing.
    authority: djinn_cgroup_launcher::LeaseAuthority,
    /// The absolute queue deadline this invocation sent, and the clock it is
    /// judged against.
    ///
    /// Both are logged because the reason a status read reports is
    /// structurally lossy: the coordinator terminalizes an expired queue row as
    /// `deadline_expired`, but `LeaseStatus` carries no terminal reason, so the
    /// pod sees a bare `Cancelled`. `now_ms >= queue_deadline_ms > 0` is the
    /// evidence that distinguishes "its queue position expired" from "somebody
    /// cancelled it".
    queue_deadline_ms: i64,
    now_ms: i64,
}

/// Closed enumeration naming why this invocation can never hold the lease.
/// Bounded because it is also a metric label.
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
fn degrade_reason(result: &LeaseResult) -> &'static str {
    match result {
        // The coordinator terminalizes an expired queue row as
        // `deadline_expired`; a wait that ends without a grant reports it as a
        // timeout here and as a cancelled terminal state on a later status read.
        LeaseResult::LeaseWaitTimeout { .. } => "deadline_expired",
        LeaseResult::Cancelled { .. }
        | LeaseResult::Status(LeaseStatus {
            state: LeaseState::Cancelled,
            ..
        }) => "cancelled",
        LeaseResult::Released { .. }
        | LeaseResult::Status(LeaseStatus {
            state: LeaseState::Released,
            ..
        }) => "released",
        LeaseResult::Abandoned { .. } => "abandoned",
        LeaseResult::LeaseUnavailable => "lease_unavailable",
        _ => "unclassified",
    }
}

/// What "degraded" means for a leaf born under `authority`.
#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
fn degraded_quota(authority: djinn_cgroup_launcher::LeaseAuthority) -> &'static str {
    match authority {
        djinn_cgroup_launcher::LeaseAuthority::Armed => "launcher_unleased",
        djinn_cgroup_launcher::LeaseAuthority::Unarmed => "unrestricted",
    }
}

#[cfg_attr(not(test), allow(dead_code))] // consumed by the workspace lease wiring task
fn lease_failure_classify(
    result: LeaseResult,
    deadline: &mut std::time::Instant,
    credit_used: &mut bool,
    output: &mut Option<LeaseInvocationError>,
    unleased: &mut bool,
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
        // Queue timeout (credit spent or never issued), and the terminal
        // records a timed-out queue leaves behind: the coordinator terminalizes
        // an expired row as `deadline_expired`, which a later status read
        // reports as a cancelled terminal state. Both mean the same thing —
        // this invocation will never hold the lease — so both degrade.
        LeaseResult::LeaseWaitTimeout { .. }
        | LeaseResult::Cancelled { .. }
        | LeaseResult::Released { .. }
        | LeaseResult::Abandoned { .. }
        | LeaseResult::Status(LeaseStatus {
            state: LeaseState::Cancelled | LeaseState::Released,
            ..
        }) => *unleased = true,
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
    use djinn_core::clock::TestClock;
    use std::future::pending;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[path = "process_lease_tests.rs"]
    mod lease_runner_tests;

    /// Small launcher double used by the lease-state-machine tests. It keeps
    /// the cgroup boundary observable without consulting command strings.
    #[derive(Default)]
    struct FakeLauncher {
        child_pid: Mutex<Option<u32>>,
        lifts: Mutex<Vec<LeaseFencingToken>>,
        kills: AtomicUsize,
        empties: AtomicUsize,
    }

    impl CgroupLauncherClient for FakeLauncher {
        fn launch(
            &self,
            mut command: Command,
            _: &TaskInvocationLeaseIdentity,
            _authority: djinn_cgroup_launcher::LeaseAuthority,
        ) -> io::Result<Box<dyn ProcessHandle>> {
            let child = command.spawn()?;
            *self.child_pid.lock().unwrap() = Some(child.id());
            Ok(Box::new(child))
        }
    }
    impl FakeLauncher {
        fn fenced_lift(
            &self,
            _: &TaskInvocationLeaseIdentity,
            token: &LeaseFencingToken,
        ) -> io::Result<()> {
            self.lifts.lock().unwrap().push(token.clone());
            Ok(())
        }
        fn kill(&self, _: &TaskInvocationLeaseIdentity) -> io::Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn wait_empty(&self, _: &TaskInvocationLeaseIdentity) -> io::Result<()> {
            self.empties.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn test_identity() -> TaskInvocationLeaseIdentity {
        TaskInvocationLeaseIdentity {
            task_id: "task".into(),
            task_run_id: "run".into(),
            invocation_id: "invocation".into(),
        }
    }

    /// The reporting context every `lease_failure` call carries. These unit
    /// tests exercise the CLASSIFICATION; the emitted diagnostic itself is
    /// asserted end-to-end in `process/tests/process_lease_queue_deadline_tests`.
    fn degrade_diagnostic(identity: &TaskInvocationLeaseIdentity) -> DegradeDiagnostic<'_> {
        DegradeDiagnostic {
            identity,
            observed_usage_usec: 52_800_000,
            authority: djinn_cgroup_launcher::LeaseAuthority::Armed,
            queue_deadline_ms: 0,
            now_ms: 0,
        }
    }

    /// A long-lived fixture child, contained the way the production handle
    /// contains a leaf: its own process group, torn down as a group. Spawning it
    /// through `sh -c` left the real command as a grandchild on any runner whose
    /// `/bin/sh` forks rather than execs, and the direct-child kill orphaned it
    /// holding this test process's stdout — a nextest `LEAK`.
    fn fixture_sleep() -> std::process::Child {
        let mut command = Command::new("sleep");
        command.arg("1");
        fixture_child::isolate_group(&mut command);
        command.spawn().unwrap()
    }

    /// A paused service future must lose to irreversible cancellation, so its
    /// grant cannot reach the lift path (terminal-before-grant ordering).
    #[tokio::test]
    async fn paused_grant_loses_to_terminal_intent() {
        let mut child = fixture_sleep();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let clock = TestClock::new(std::time::SystemTime::UNIX_EPOCH, std::time::Instant::now());
        let result = await_lease_or_terminal(
            pending::<LeaseResult>(),
            &mut child,
            &cancel,
            &clock,
            clock.now_instant() + Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            LeaseWait::Terminal((_, ProcessTermination::Cancelled))
        ));
        fixture_child::kill_group(&mut child).unwrap();
        fixture_child::wait_group_empty(&mut child).unwrap();
    }

    /// The injected fake clock controls timeout while a service response is paused.
    #[tokio::test]
    async fn fake_clock_times_out_paused_service() {
        let mut child = fixture_sleep();
        let cancel = CancellationToken::new();
        let base = std::time::Instant::now();
        let clock = TestClock::new(std::time::SystemTime::UNIX_EPOCH, base);
        clock.advance_mono(Duration::from_secs(2));
        let result =
            await_lease_or_terminal(pending::<LeaseResult>(), &mut child, &cancel, &clock, base)
                .await
                .unwrap();
        assert!(matches!(
            result,
            LeaseWait::Terminal((_, ProcessTermination::TimedOut))
        ));
        fixture_child::kill_group(&mut child).unwrap();
        fixture_child::wait_group_empty(&mut child).unwrap();
    }

    #[test]
    fn typed_lease_failures_remain_distinct() {
        let mut deadline = std::time::Instant::now();
        let mut used = false;
        let mut error = None;
        let mut unleased = false;
        lease_failure(
            degrade_diagnostic(&test_identity()),
            LeaseResult::LeaseIdentityConflict {
                identity: LeaseIdentity::TaskInvocation(test_identity()),
            },
            &mut deadline,
            &mut used,
            &mut error,
            &mut unleased,
        );
        assert!(matches!(
            error,
            Some(LeaseInvocationError::LeaseIdentityConflict)
        ));
        error = None;
        lease_failure(
            degrade_diagnostic(&test_identity()),
            LeaseResult::LeaseUnavailable,
            &mut deadline,
            &mut used,
            &mut error,
            &mut unleased,
        );
        assert!(matches!(
            error,
            Some(LeaseInvocationError::LeaseUnavailable)
        ));
        assert!(
            !unleased,
            "an unusable lease authority is a failure, never a degrade"
        );
    }

    /// A queue that ends without capacity degrades to unleased execution: no
    /// error is produced for the caller to kill the child with. Every response
    /// shape a lost queue can take — the timeout itself and the terminal record
    /// the coordinator leaves behind — degrades identically.
    #[test]
    fn lost_queue_responses_degrade_without_error() {
        for result in [
            LeaseResult::LeaseWaitTimeout {
                timeout_credit: None,
            },
            LeaseResult::Cancelled {
                candidate_cleanup: false,
            },
            LeaseResult::Released {
                candidate_cleanup: false,
            },
            LeaseResult::Abandoned {
                candidate_cleanup: false,
            },
            LeaseResult::Status(LeaseStatus {
                state: LeaseState::Cancelled,
                fencing_token: None,
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
                pod_uid: None,
                candidate_cleanup: false,
            }),
        ] {
            let mut deadline = std::time::Instant::now();
            let original = deadline;
            let mut used = true;
            let mut error = None;
            let mut unleased = false;
            lease_failure(
                degrade_diagnostic(&test_identity()),
                result.clone(),
                &mut deadline,
                &mut used,
                &mut error,
                &mut unleased,
            );
            assert!(error.is_none(), "{result:?} must not fail the invocation");
            assert!(unleased, "{result:?} must degrade to unleased execution");
            assert_eq!(deadline, original);
        }
    }

    /// A queued row that has not lost the queue keeps waiting: no degrade, no
    /// error. Without this the runner would abandon the lease on its first
    /// still-queued poll.
    #[test]
    fn still_queued_responses_neither_fail_nor_degrade() {
        let mut deadline = std::time::Instant::now();
        let mut used = false;
        let mut error = None;
        let mut unleased = false;
        lease_failure(
            degrade_diagnostic(&test_identity()),
            LeaseResult::Status(LeaseStatus {
                state: LeaseState::Queued,
                fencing_token: None,
                deadlines: LeaseDeadlines {
                    queue_deadline_ms: 0,
                    launch_deadline_ms: 0,
                },
                pod_uid: None,
                candidate_cleanup: false,
            }),
            &mut deadline,
            &mut used,
            &mut error,
            &mut unleased,
        );
        assert!(error.is_none());
        assert!(!unleased);
    }

    #[test]
    fn timeout_credit_extends_deadline_once() {
        let mut deadline = std::time::Instant::now();
        let original = deadline;
        let mut used = false;
        let mut error = None;
        let credit = LeaseResult::LeaseWaitTimeout {
            timeout_credit: Some(djinn_supervisor::services::TimeoutCredit {
                units: 1,
                retry_after_ms: 25,
            }),
        };
        let mut unleased = false;
        lease_failure(
            degrade_diagnostic(&test_identity()),
            credit.clone(),
            &mut deadline,
            &mut used,
            &mut error,
            &mut unleased,
        );
        assert_eq!(deadline, original + Duration::from_millis(25));
        assert!(!unleased, "the one credited retry still waits for capacity");
        lease_failure(
            degrade_diagnostic(&test_identity()),
            credit,
            &mut deadline,
            &mut used,
            &mut error,
            &mut unleased,
        );
        assert_eq!(deadline, original + Duration::from_millis(25));
        assert!(error.is_none());
        assert!(
            unleased,
            "the spent credit degrades to unleased instead of failing"
        );
    }

    #[test]
    fn fake_launcher_records_cgroup_lifecycle() {
        let launcher = FakeLauncher::default();
        let identity = test_identity();
        launcher
            .fenced_lift(&identity, &LeaseFencingToken(7))
            .unwrap();
        launcher.kill(&identity).unwrap();
        launcher.wait_empty(&identity).unwrap();
        assert_eq!(*launcher.lifts.lock().unwrap(), vec![LeaseFencingToken(7)]);
        assert_eq!(launcher.kills.load(Ordering::SeqCst), 1);
        assert_eq!(launcher.empties.load(Ordering::SeqCst), 1);
    }

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
