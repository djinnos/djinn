//! Broker-backed remote child lifecycle for lease invocations.

use std::io;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use djinn_cgroup_launcher::{
    CommandSpec, CpuStat, Invocation, LeaseAuthority, broker::ChildStatus,
    transport::UnixBrokerClient,
};
use djinn_supervisor::services::{InvocationLiftDecision, TaskInvocationLeaseIdentity};

/// Project the durable admission decision onto the authority the leaf is BORN
/// under.
///
/// The two `Armed` arms are deliberately different intents that share one birth
/// quota: `Lift` will raise it on a matching fenced grant, and `Shadow` observes
/// without lifting (documented to clamp on purpose). `Unleased` is the arm that
/// cost four production rollouts — the durable invocation-lease authority is
/// absent or disarmed, no grant can ever authorize a lift, and clamping such
/// an invocation to 250m is pure loss. It maps to `Unarmed` so the leaf is born
/// with no quota of its own.
pub(crate) fn birth_authority(decision: InvocationLiftDecision) -> LeaseAuthority {
    match decision {
        InvocationLiftDecision::Lift | InvocationLiftDecision::Shadow => LeaseAuthority::Armed,
        InvocationLiftDecision::Unleased => LeaseAuthority::Unarmed,
    }
}

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

/// Remote process lifecycle owned by the authenticated launcher broker.
pub(crate) trait ProcessHandle: Send {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>>;
    fn drain_stderr(&mut self) -> io::Result<Vec<u8>>;
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
    fn sample_cpu(&mut self) -> io::Result<CpuStat>;
    /// Apply the one-way cgroup lift to the leaf THIS handle owns.
    ///
    /// # Why this takes no fencing token (goxi launcher blocker 14)
    ///
    /// There are two different fences in this system and they were conflated
    /// here, which broke every escalation in production:
    ///
    /// * the **broker invocation fence** — declared by the worker in the
    ///   `BEGIN` control, stored on the leaf at birth, and the only value
    ///   `Launcher::fenced_lift` will accept on a `LIFT`; and
    /// * the **durable lease fencing token** — minted by the coordinator from
    ///   `build_lease_fencing_token_seq` at grant time, i.e. long after the leaf
    ///   exists, and therefore something the birth control cannot possibly have
    ///   named.
    ///
    /// The composition passed a launcher-wide constant `0` to `BEGIN` and the
    /// coordinator's token to `LIFT`. The sequence starts at 1, so the two could
    /// never be equal: every lift was refused with `FenceMismatch`, reported to
    /// the worker as the indistinguishable `InvalidControl`, and (because the
    /// error failed the command) took the agent's `shell` tool down with it.
    ///
    /// The durable token still governs **whether** a lift may happen — the
    /// runner only reaches this call inside a `LeaseResult::Bound` arm whose
    /// `fencing_token` matches the grant it validated, and it writes that token
    /// to the invocation journal first. It just is not the value the cgroup
    /// boundary is fenced by, so it is not accepted here: an implementation that
    /// silently ignored an argument would be the same defect wearing a
    /// signature.
    fn fenced_lift(&mut self) -> io::Result<()>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait_empty(&mut self) -> io::Result<()>;
    fn cleanup(&mut self) -> io::Result<()>;
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait CgroupLauncherClient: Send + Sync + 'static {
    /// Create the invocation leaf and spawn the child into it.
    ///
    /// `authority` is not advisory: it selects the leaf's birth `cpu.max`, which
    /// is written before the child may `execve` and cannot be revised
    /// afterwards. Callers must therefore resolve the durable admission decision
    /// BEFORE calling this, not after the queue.
    fn launch(
        &self,
        command: Command,
        identity: &TaskInvocationLeaseIdentity,
        authority: LeaseAuthority,
    ) -> io::Result<Box<dyn ProcessHandle>>;
}

/// The broker invocation fence for one invocation identity.
///
/// # Why it is derived rather than configured (goxi launcher blocker 14)
///
/// `Launcher::fenced_lift` accepts exactly the fence the leaf was born with, and
/// the leaf is born inside `BEGIN`/`CREATE` — before any lease exists. So the
/// fence has to be a value the *worker* can name at birth and name again,
/// identically, at lift time. It used to be a launcher-wide constant supplied at
/// construction (`UnixBrokerLauncher::new(client, 0)`), while the lift presented
/// the coordinator's durable token; see [`ProcessHandle::fenced_lift`] for the
/// full account of why those two can never agree.
///
/// Deriving it from the invocation identity gives the property the contract
/// actually needs: **the birth quota and the lift name the same invocation**.
/// The id is the `uuid::Uuid::now_v7()` the runner mints per invocation and is
/// also the leaf name the broker binds every control to, so two concurrent
/// invocations in one pod get two different fences and a lift carrying the wrong
/// one is refused rather than silently applied to the wrong leaf.
///
/// # Why a digest of the whole id
///
/// A UUIDv7's high 64 bits are a 48-bit millisecond timestamp plus a short
/// counter, so invocations minted inside the same millisecond share them —
/// measured, 64 ids collapsed to 2 values. Folding the ENTIRE id keeps the
/// distinctness the contract needs. This is a collision-avoidance aid over the
/// handful of invocations live in one pod at once, not a security primitive:
/// the broker's actual authority is the authenticated peer, the per-invocation
/// binding and the rotating control nonce.
pub(crate) fn invocation_fence(invocation_id: &str) -> u64 {
    // FNV-1a, 64-bit. Deterministic and dependency-free, so `BEGIN` and `LIFT`
    // derive the same value from the same identity with no shared state.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in invocation_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// Production-only Unix broker adapter; it has no local `Command::spawn` fallback.
pub(crate) struct UnixBrokerLauncher {
    client: Arc<Mutex<UnixBrokerClient>>,
}

impl UnixBrokerLauncher {
    #[allow(dead_code)] // constructed by workspace broker composition
    pub(crate) fn new(client: UnixBrokerClient) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
        }
    }
}

impl CgroupLauncherClient for UnixBrokerLauncher {
    fn launch(
        &self,
        command: Command,
        identity: &TaskInvocationLeaseIdentity,
        authority: LeaseAuthority,
    ) -> io::Result<Box<dyn ProcessHandle>> {
        let spec = command_spec(command)?;
        let id = identity.invocation_id.clone();
        // ONE source for the fence, read once and carried into the handle. The
        // `BEGIN` below and the `LIFT` in `fenced_lift` must present the same
        // value or the privileged broker refuses the lift, so there must be no
        // second place that decides it.
        let fence = invocation_fence(&id);
        let mut client = lock_client(&self.client)?;
        client
            .begin(Invocation {
                id: id.clone(),
                fence,
            })
            .map_err(broker_error)?;
        if let Err(error) = client.create(&id, &id, authority, &spec) {
            let _ = client.cleanup(&id);
            return Err(broker_error(error));
        }
        Ok(Box::new(UnixBrokerProcessHandle {
            client: self.client.clone(),
            id,
            fence,
            status: None,
            stdout: Vec::new(),
        }))
    }
}

struct UnixBrokerProcessHandle {
    client: Arc<Mutex<UnixBrokerClient>>,
    id: String,
    /// The fence this invocation was BEGUN with. Copied from the same value the
    /// `BEGIN` control carried so the lift cannot disagree with the birth.
    fence: u64,
    status: Option<ExitStatus>,
    // Status polling uses the broker's stdout operation, which drains its
    // accumulated stream buffer. Preserve those bytes for the next drain.
    stdout: Vec<u8>,
}

impl UnixBrokerProcessHandle {
    fn record(&mut self, status: ChildStatus) -> io::Result<()> {
        if status != ChildStatus::Running {
            self.status = Some(remote_exit_status(status)?);
        }
        Ok(())
    }

    fn output(&mut self, stderr: bool) -> io::Result<Vec<u8>> {
        let (bytes, _, status) = if stderr {
            lock_client(&self.client)?
                .stderr(&self.id)
                .map_err(broker_error)?
        } else {
            lock_client(&self.client)?
                .stdout(&self.id)
                .map_err(broker_error)?
        };
        self.record(status)?;
        Ok(bytes)
    }
}

impl ProcessHandle for UnixBrokerProcessHandle {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        let mut output = std::mem::take(&mut self.stdout);
        output.extend(self.output(false)?);
        Ok(output)
    }

    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        self.output(true)
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            let (output, _, status) = lock_client(&self.client)?
                .stdout(&self.id)
                .map_err(broker_error)?;
            self.stdout.extend(output);
            self.record(status)?;
        }
        Ok(self.status)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        lock_client(&self.client)?
            .sample(&self.id)
            .map_err(broker_error)
    }

    fn fenced_lift(&mut self) -> io::Result<()> {
        let fence = self.fence;
        lock_client(&self.client)?
            .lift(&self.id, fence)
            .map_err(broker_error)
    }

    fn kill(&mut self) -> io::Result<()> {
        lock_client(&self.client)?
            .kill(&self.id)
            .map_err(broker_error)
    }

    fn wait_empty(&mut self) -> io::Result<()> {
        lock_client(&self.client)?
            .wait_empty(&self.id)
            .map_err(broker_error)
    }

    fn cleanup(&mut self) -> io::Result<()> {
        lock_client(&self.client)?
            .cleanup(&self.id)
            .map_err(broker_error)
    }
}

fn lock_client(
    client: &Arc<Mutex<UnixBrokerClient>>,
) -> io::Result<std::sync::MutexGuard<'_, UnixBrokerClient>> {
    client
        .lock()
        .map_err(|_| io::Error::other("launcher broker client mutex poisoned"))
}

fn broker_error(error: djinn_cgroup_launcher::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(unix)]
fn remote_exit_status(status: ChildStatus) -> io::Result<ExitStatus> {
    match status {
        ChildStatus::Running => Err(io::Error::other("running child is not terminal")),
        ChildStatus::Exited(code) => Ok(ExitStatus::from_raw(i32::from(code) << 8)),
        ChildStatus::Signaled(signal) => Ok(ExitStatus::from_raw(i32::from(signal))),
    }
}

#[cfg(not(unix))]
fn remote_exit_status(_: ChildStatus) -> io::Result<ExitStatus> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unix broker status",
    ))
}

pub(crate) fn command_spec(command: Command) -> io::Result<CommandSpec> {
    let text = |value: &std::ffi::OsStr, what| {
        value.to_str().map(str::to_owned).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{what} must be UTF-8"))
        })
    };
    let named = text(command.get_program(), "program")?;
    let argv = command
        .get_args()
        .map(|argument| text(argument, "argument"))
        .collect::<io::Result<Vec<_>>>()?;
    let cwd = command
        .get_current_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "command cwd is required"))?;
    let cwd = text(cwd.as_os_str(), "cwd")?;
    let environment = crate::environment::admitted_child_environment(&command)?;
    let program = resolve_program(&named, &environment)?;
    let spec = CommandSpec {
        program,
        argv,
        cwd,
        environment,
    };
    spec.validate().map_err(|error| {
        // Name what was rejected. The seventh launcher blocker cost a full
        // investigation because the only thing production could say was
        // "lease invocation failed: InvalidCommand", which does not
        // distinguish the program from the cwd from a single environment
        // entry — and `InvalidCommand` is deliberately coarse on the wire
        // (the privileged broker does not describe its refusals to callers).
        // This is the worker's own copy of the same check, so it can.
        io::Error::other(format!(
            "{error:?} ({error}): program {:?} (named {named:?}), cwd {:?}, \
             {} environment entries",
            spec.program,
            spec.cwd,
            spec.environment.len()
        ))
    })?;
    Ok(spec)
}

/// Resolve what the caller *named* into the absolute path `execve` requires.
///
/// # Why the conversion has to do this (task goxi, seventh launcher blocker)
///
/// [`std::process::Command`] documents a `PATH` search for a program with no
/// `/` in it, and every caller in this repo relies on that:
/// `extension::handlers::workspace` builds `Command::new("bash").arg("-lc")`,
/// which is the **first thing an armed task pod runs**. [`CommandSpec`] is not a
/// `Command`; it is an `execve(2)` request, and `execve` performs no search at
/// all. So this conversion silently dropped a documented promise, twice over:
///
/// * `CommandSpec::validate` requires an absolute path from a known directory,
///   so a bare name was refused before a leaf or a child existed — measured, on
///   the real launcher, every brokered `run_shell` failed
///   `lease invocation failed: InvalidCommand`;
/// * and had it passed, `spawn.rs`'s `execve(prepared.program, …)` would have
///   returned `ENOENT` and the child would have `_exit`ed 127 without running.
///
/// Resolving here rather than at one call site is the point. Fixing
/// `workspace.rs` to say `/bin/bash` would leave the conversion just as lossy
/// for the next `Command::new("git")` anyone writes, and that next caller would
/// be blocker number nine.
///
/// # Which `PATH`
///
/// The child's, taken from `environment` — which is exactly what `Command` does
/// on Unix (a `PATH` set on the command wins over the parent's for the search).
/// The fallback when the child has no `PATH` is `execvp(3)`'s own
/// `confstr(_CS_PATH)` default, for the same reason.
///
/// # Which filesystem
///
/// The worker's. A brokered child executes in the *launcher* container's mount
/// namespace, not the worker's, so in principle the two could disagree — but
/// both containers run the same image (`/opt/djinn/bin/djinn-agent-worker` and
/// `/opt/djinn/bin/djinn-cgroup-launcher` are siblings in it), and a
/// disagreement fails loudly at `execve` with exit 127 rather than silently.
/// Doing the search inside the privileged broker instead would put a filesystem
/// walk in the process whose entire design is to accept a bounded data-only
/// request, and would buy nothing: the control is
/// `CommandSpec::validate`'s allow-list applied to the resolved path, and that
/// runs broker-side either way.
fn resolve_program(named: &str, environment: &[(String, String)]) -> io::Result<String> {
    // `execvp(3)`: a program containing a slash is a path and is used verbatim.
    // A *relative* path stays relative here on purpose — resolving it against
    // the worker's cwd would invent an authority the caller never asked for,
    // and `CommandSpec::validate` refuses it a moment later.
    if named.contains('/') {
        return Ok(named.to_owned());
    }
    if named.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "brokered command names no program",
        ));
    }

    let path = environment
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.as_str())
        .unwrap_or(DEFAULT_EXEC_PATH);

    for directory in path.split(':') {
        // `execvp` treats an empty PATH element as the current directory.
        let directory = if directory.is_empty() { "." } else { directory };
        let candidate = format!("{}/{named}", directory.trim_end_matches('/'));
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "brokered command names program {named:?}, which is on no directory of the child \
             PATH {path:?}; `execve` performs no PATH search, so this would have exec'd nothing"
        ),
    ))
}

/// `execvp(3)`'s search path when the environment carries none.
const DEFAULT_EXEC_PATH: &str = "/usr/local/bin:/bin:/usr/bin";

#[cfg(unix)]
fn is_executable_file(candidate: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(candidate).is_ok_and(|metadata| {
        // `metadata` follows symlinks, which is required rather than incidental:
        // the production `bash` is reached through one (`/bin` -> `usr/bin`).
        // `!is_dir()` rather than `is_file()` so a device or fifo is judged by
        // its mode, exactly as `execvp` would.
        !metadata.is_dir() && metadata.permissions().mode() & 0o111 != 0
    })
}

#[cfg(not(unix))]
fn is_executable_file(candidate: &str) -> bool {
    std::fs::metadata(candidate).is_ok_and(|metadata| !metadata.is_dir())
}

/// The containment a launcher double owes the fixture children it spawns.
///
/// # Why a bare `std::process::Child` is not a leaf cgroup
///
/// [`ProcessHandle::kill`] is `cgroup.kill` in production: it reaches EVERY
/// process in the invocation's leaf, children of the command included. And
/// [`ProcessHandle::wait_empty`] returns only once the leaf reports
/// `populated 0`, i.e. once nothing from that invocation is running any more.
/// The doubles stand a bare [`std::process::Child`] in for that leaf, and a bare
/// child has neither property: `Child::kill` signals ONLY the direct child, and
/// there is nothing that waits for its descendants.
///
/// That gap is invisible wherever `/bin/sh` execs its `-c` command (bash does),
/// and it leaks a process wherever `/bin/sh` FORKS it (dash does — which is
/// `/bin/sh` on the CI runners). There, `sh -c "sleep 30"` is two processes; the
/// double killed the shell, the `sleep` was orphaned still holding the test
/// process's inherited stdout, and nextest — which waits for that pipe to close
/// after the test process exits — reported the test as `LEAK`.
///
/// So the fixtures spawn into their own process group and tear the whole group
/// down: `kill_group` is the stand-in for `cgroup.kill`, and `wait_group_empty`
/// is the stand-in for `populated 0`. A double that returned from `wait_empty`
/// while a fixture process was still alive would be asserting a containment
/// property the production handle has and the double does not.
#[cfg(test)]
pub(crate) mod fixture_child {
    use std::io;
    use std::process::{Child, Command};
    use std::time::Duration;

    /// Spawn this command as the leader of its own process group, so the whole
    /// subtree it may create stays addressable by one signal.
    pub(crate) fn isolate_group(command: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(not(unix))]
        let _ = command;
    }

    /// SIGKILL the child's whole process group — the double's `cgroup.kill`.
    pub(crate) fn kill_group(child: &mut Child) -> io::Result<()> {
        #[cfg(unix)]
        {
            // A reaped pid can be recycled, so signalling by pid is only sound
            // while the leader is still un-reaped. `try_wait` reaps an exited
            // leader, and once it has, the group can no longer be named here.
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            let pid = i32::try_from(child.id())
                .map_err(|_| io::Error::other("fixture child pid does not fit in pid_t"))?;
            // SAFETY: `kill(2)` with a negated pid signals the process group led
            // by that pid. The fixture children are group leaders
            // (`isolate_group`), so the group is exactly this child's subtree.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
                // No such group (the child was spawned without isolation, or the
                // group is already gone): fall back to the direct child.
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
                return child.kill();
            }
            Ok(())
        }
        #[cfg(not(unix))]
        child.kill()
    }

    /// Return only once nothing from this fixture is running — the double's
    /// `populated 0`. Reaps the leader, then waits out its group.
    pub(crate) fn wait_group_empty(child: &mut Child) -> io::Result<()> {
        #[cfg(unix)]
        let pid = i32::try_from(child.id()).ok();
        child.wait()?;
        #[cfg(unix)]
        {
            let Some(pid) = pid else { return Ok(()) };
            // The group was just SIGKILLed, so this settles in microseconds. The
            // bound is a safety valve only: a double must not be able to hang a
            // test run forever.
            for _ in 0..10_000 {
                // SAFETY: signal 0 performs the existence check only.
                if unsafe { libc::kill(-pid, 0) } != 0 {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(io::Error::other(
                "fixture process group never emptied after SIGKILL",
            ))
        }
        #[cfg(not(unix))]
        Ok(())
    }
}

#[cfg(test)]
impl ProcessHandle for std::process::Child {
    fn drain_stdout(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn drain_stderr(&mut self) -> io::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        std::process::Child::try_wait(self)
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        std::process::Child::wait(self)
    }

    fn sample_cpu(&mut self) -> io::Result<CpuStat> {
        Ok(CpuStat {
            usage_usec: 10,
            ..CpuStat::default()
        })
    }

    fn fenced_lift(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn kill(&mut self) -> io::Result<()> {
        fixture_child::kill_group(self)
    }

    fn wait_empty(&mut self) -> io::Result<()> {
        fixture_child::wait_group_empty(self)
    }

    fn cleanup(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod broker_environment_tests {
    use super::*;

    /// The pod's build environment must survive the broker hop, or a brokered
    /// build runs cold, in the wrong target directory, without mold — the exact
    /// regression that made routing through the launcher a net loss.
    #[test]
    fn the_inherited_build_environment_reaches_the_child() {
        // SAFETY: single-threaded test, and both keys are test-local.
        unsafe {
            std::env::set_var("CARGO_TARGET_DIR", "/cache/cargo-target/proj/mold-jobs-4");
            std::env::set_var("DJINN_BROKER_ENV_TEST_SECRET", "kept");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "must-not-leak");
        }

        let mut command = Command::new("/bin/sh");
        command.current_dir("/workspace");
        let spec = command_spec(command).expect("spec");
        let value = |key: &str| {
            spec.environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(
            value("CARGO_TARGET_DIR"),
            Some("/cache/cargo-target/proj/mold-jobs-4"),
            "the warm cache directory must reach a brokered build"
        );
        // The allow-list is still closed: an unlisted key is not forwarded.
        assert_eq!(value("AWS_SECRET_ACCESS_KEY"), None);

        unsafe {
            std::env::remove_var("CARGO_TARGET_DIR");
            std::env::remove_var("DJINN_BROKER_ENV_TEST_SECRET");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
    }

    /// An explicitly-set value wins over the inherited one, and `env_remove`
    /// really removes rather than being ignored.
    #[test]
    fn explicit_command_environment_overrides_and_removes() {
        // SAFETY: single-threaded test, test-local key.
        unsafe { std::env::set_var("RUST_LOG", "inherited") };

        let mut command = Command::new("/bin/sh");
        command.current_dir("/workspace");
        command.env("RUST_LOG", "explicit");
        command.env("CARGO_INCREMENTAL", "0");
        command.env_remove("TERM");
        let spec = command_spec(command).expect("spec");
        let value = |key: &str| {
            spec.environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };

        assert_eq!(value("RUST_LOG"), Some("explicit"));
        assert_eq!(value("CARGO_INCREMENTAL"), Some("0"));
        assert_eq!(value("TERM"), None);

        unsafe { std::env::remove_var("RUST_LOG") };
    }
}
