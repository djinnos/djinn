//! Fail-closed launcher for reusable final verification on Linux.
//!
//! This module intentionally does not use [`crate::SANDBOX`] or the agent-shell
//! policy.  Those APIs may select a heuristic fallback and grant broad host
//! reads/writes, neither of which is acceptable evidence for reusable
//! verification.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use djinn_core::clock::{Clock, SystemClock};
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus,
};

const REQUIRED_LANDLOCK_ABI: i32 = ABI::V3 as i32;
// `Command` reports a `pre_exec` failure to its parent as an OS error. Reserve
// an errno which the launcher itself never otherwise returns so isolation setup
// remains distinguishable from an ordinary spawn error.
const ISOLATION_SETUP_ERROR: i32 = libc::ENOTRECOVERABLE;

/// One immutable strict-catalog localhost listener for an executing attempt.
/// Commands can connect only to this fixed port; no command bytes describe a
/// host or destination port.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FinalVerificationLoopbackEndpoint {
    pub preset_id: String,
    pub port: u16,
}

/// Attempt-owned, immutable fixed-port network policy.
///
/// It is constructed outside the command loop so invalid catalog material is a
/// bounded infrastructure failure, not command evidence.
#[derive(Debug)]
pub struct FinalVerificationNetworkSession {
    endpoints: Vec<FinalVerificationLoopbackEndpoint>,
    // CLOEXEC descriptors for the keeper-owned attempt namespaces. Verification
    // processes use them only during pre_exec and cannot retain them.
    user_namespace: std::fs::File,
    network_namespace: std::fs::File,
    keeper_pid: libc::pid_t,
    broker_shutdown: Arc<AtomicBool>,
    broker_threads: Mutex<Vec<JoinHandle<()>>>,
    // Retained only for the in-module regression test that asserts the
    // anonymous broker channels are not inherited across command exec.
    #[cfg(test)]
    broker_fd_numbers: Vec<RawFd>,
}

impl FinalVerificationNetworkSession {
    pub fn create(
        mut endpoints: Vec<FinalVerificationLoopbackEndpoint>,
    ) -> Result<Self, FinalVerificationError> {
        endpoints.sort();
        let mut ports = std::collections::BTreeSet::new();
        for endpoint in &endpoints {
            if endpoint.preset_id.is_empty() || endpoint.port == 0 || !ports.insert(endpoint.port) {
                return Err(FinalVerificationError::BackendUnavailable {
                    reason: "strict catalog loopback endpoints are invalid",
                });
            }
        }
        ensure_backend_available()?;
        let (ready_read, ready_write) =
            pipe_cloexec().map_err(|_| FinalVerificationError::BackendUnavailable {
                reason: "attempt network session pipe setup failed",
            })?;
        let mut channels = Vec::with_capacity(endpoints.len());
        for endpoint in &endpoints {
            let (parent, child) =
                seqpacket_pair().map_err(|_| FinalVerificationError::BackendUnavailable {
                    reason: "attempt network broker setup failed",
                })?;
            channels.push((endpoint.clone(), parent, child));
        }
        // SAFETY: the child makes only syscall-backed setup calls and never
        // returns into Rust after fork.
        let keeper_pid = unsafe { libc::fork() };
        if keeper_pid < 0 {
            close_fd(ready_read);
            close_fd(ready_write);
            for (_, parent, child) in channels {
                close_fd(parent);
                close_fd(child);
            }
            return Err(FinalVerificationError::BackendUnavailable {
                reason: "attempt network session fork failed",
            });
        }
        if keeper_pid == 0 {
            close_fd(ready_read);
            for (_, parent, _) in &channels {
                close_fd(*parent);
            }
            keeper_main(ready_write, channels);
        }
        close_fd(ready_write);
        for (_, _, child) in &channels {
            close_fd(*child);
        }
        let ready = read_ready(ready_read);
        close_fd(ready_read);
        if !ready {
            for (_, parent, _) in channels {
                close_fd(parent);
            }
            wait_for_pid(keeper_pid);
            return Err(FinalVerificationError::BackendUnavailable {
                reason: "attempt network listener setup failed",
            });
        }
        let user_namespace = open_namespace(keeper_pid, "user").map_err(|_| {
            FinalVerificationError::BackendUnavailable {
                reason: "attempt user namespace setup failed",
            }
        })?;
        let network_namespace = open_namespace(keeper_pid, "net").map_err(|_| {
            FinalVerificationError::BackendUnavailable {
                reason: "attempt network namespace setup failed",
            }
        })?;
        let broker_shutdown = Arc::new(AtomicBool::new(false));
        let mut broker_threads = Vec::with_capacity(channels.len());
        #[cfg(test)]
        let mut broker_fd_numbers = Vec::with_capacity(channels.len());
        for (endpoint, parent, _) in channels {
            #[cfg(test)]
            broker_fd_numbers.push(parent);
            let shutdown = Arc::clone(&broker_shutdown);
            broker_threads.push(std::thread::spawn(move || {
                broker_main(parent, endpoint.port, shutdown)
            }));
        }
        Ok(Self {
            endpoints,
            user_namespace,
            network_namespace,
            keeper_pid,
            broker_shutdown,
            broker_threads: Mutex::new(broker_threads),
            #[cfg(test)]
            broker_fd_numbers,
        })
    }

    pub fn endpoints(&self) -> &[FinalVerificationLoopbackEndpoint] {
        &self.endpoints
    }
}

impl Drop for FinalVerificationNetworkSession {
    fn drop(&mut self) {
        self.broker_shutdown.store(true, Ordering::Release);
        // The keeper owns every child listener, so SIGTERM closes listeners
        // even when it is blocked in accept(2).
        unsafe { libc::kill(self.keeper_pid, libc::SIGTERM) };
        wait_for_pid(self.keeper_pid);
        if let Ok(mut threads) = self.broker_threads.lock() {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
    }
}

/// An argv invocation and the complete set of host paths it may use.
///
/// `tool_runtime` must include the executable and every runtime directory it
/// needs (for example `/usr`, `/bin`, `/lib`, and `/lib64`).  `output_directories`
/// are relative to `worktree`; every one must be absent before launch and is
/// created empty immediately before the child is spawned.
#[derive(Debug, Clone)]
pub struct FinalVerificationRequest {
    pub argv: Vec<String>,
    pub worktree: PathBuf,
    pub working_directory: PathBuf,
    pub tool_runtime: Vec<PathBuf>,
    pub read_only_external_mounts: Vec<PathBuf>,
    pub output_directories: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
}

/// Launch under an attempt-owned fixed-port session. The session table is
/// immutable and checked before the command is admitted.
pub fn launch_final_verification_in_network_session_with_timeout(
    request: FinalVerificationRequest,
    session: &FinalVerificationNetworkSession,
    timeout: Duration,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    launch_with_backend_and_setup(request, ensure_backend_available, Some(timeout), {
        let user_namespace = session.user_namespace.as_raw_fd();
        let network_namespace = session.network_namespace.as_raw_fd();
        move |worktree, runtimes, externals, outputs| {
            join_namespace_fds(user_namespace, network_namespace).map_err(|_| ())?;
            apply_filesystem_policy(worktree, runtimes, externals, outputs).map_err(|_| ())
        }
    })
}

/// Observed child status and captured output.  A non-success status is command
/// evidence, rather than a successful verification result.
#[derive(Debug)]
pub struct FinalVerificationResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl FinalVerificationResult {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// A policy violation detected before a child is permitted to execute.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FinalVerificationViolation {
    #[error("argv must contain a non-empty executable and no NUL bytes")]
    InvalidArgv,
    #[error("{field} is not an existing directory: {path}")]
    InvalidDirectory { field: &'static str, path: PathBuf },
    #[error("working directory escapes the worktree: {path}")]
    WorkingDirectoryOutsideWorktree { path: PathBuf },
    #[error("output directory must be a non-empty relative worktree path: {path}")]
    InvalidOutputDirectory { path: PathBuf },
    #[error("output-only directory was present before launch: {path}")]
    OutputOnlyPreexisting { path: PathBuf },
    #[error("output directories overlap: {first} and {second}")]
    OverlappingOutputDirectories { first: PathBuf, second: PathBuf },
}

/// Launcher failures are typed so callers can mark attempts ineligible without
/// treating a weaker execution as reusable evidence.
#[derive(Debug, thiserror::Error)]
pub enum FinalVerificationError {
    #[error("final-verification isolation backend is unavailable: {reason}")]
    BackendUnavailable { reason: &'static str },
    #[error("final-verification sandbox violation: {0}")]
    Violation(#[from] FinalVerificationViolation),
    #[error("failed to prepare output directory {path}: {source}")]
    OutputPreparation { path: PathBuf, source: io::Error },
    #[error("failed to launch isolated final verification: {0}")]
    Launch(#[source] io::Error),
}

/// Execute a final-verification command under the strict reusable policy.
///
/// Availability is checked before filesystem mutation.  There is deliberately
/// no fallback path: lack of Landlock or an isolated network namespace returns
/// [`FinalVerificationError::BackendUnavailable`]. Landlock and network
/// namespace denials are represented by the child's ordinary non-success
/// result. Linux does not expose an attributable Landlock-denial signal here,
/// so stderr text and generic permission errors are deliberately not classified
/// as typed sandbox violations.
pub fn launch_final_verification(
    request: FinalVerificationRequest,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    launch_with_backend_check(request, ensure_backend_available)
}

pub fn launch_final_verification_with_timeout(
    request: FinalVerificationRequest,
    timeout: Duration,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    launch_with_backend_check_and_timeout(request, ensure_backend_available, Some(timeout))
}

fn launch_with_backend_check(
    request: FinalVerificationRequest,
    backend_check: impl FnOnce() -> Result<(), FinalVerificationError>,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    launch_with_backend_check_and_timeout(request, backend_check, None)
}

fn launch_with_backend_check_and_timeout(
    request: FinalVerificationRequest,
    backend_check: impl FnOnce() -> Result<(), FinalVerificationError>,
    timeout: Option<Duration>,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    launch_with_backend_and_setup(
        request,
        backend_check,
        timeout,
        |worktree, runtimes, externals, outputs| {
            enter_isolated_network_namespace().map_err(|_| ())?;
            apply_filesystem_policy(worktree, runtimes, externals, outputs).map_err(|_| ())
        },
    )
}

fn launch_with_backend_and_setup(
    request: FinalVerificationRequest,
    backend_check: impl FnOnce() -> Result<(), FinalVerificationError>,
    timeout: Option<Duration>,
    isolation_setup: impl Fn(&Path, &[PathBuf], &[PathBuf], &[PathBuf]) -> Result<(), ()>
    + Send
    + Sync
    + 'static,
) -> Result<FinalVerificationResult, FinalVerificationError> {
    let prepared = PreparedRequest::new(request)?;
    // Request validation performs no mutation, so report a pre-existing output
    // as a policy violation even if the host cannot provide the backend.
    backend_check()?;
    prepared.create_empty_output_directories()?;

    let mut command = Command::new(&prepared.argv[0]);
    command
        .args(&prepared.argv[1..])
        .current_dir(&prepared.working_directory)
        .env_clear()
        .envs(&prepared.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let worktree = prepared.worktree;
    let runtimes = prepared.tool_runtime;
    let externals = prepared.read_only_external_mounts;
    let outputs = prepared.output_directories;
    // SAFETY: the closure only installs an already-described kernel policy.
    // All canonicalization and directory creation was completed in the parent.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            isolation_setup(&worktree, &runtimes, &externals, &outputs)
                .map_err(|_| isolation_setup_error())
        });
    }

    let mut child = command.spawn().map_err(|source| {
        if source.raw_os_error() == Some(ISOLATION_SETUP_ERROR) {
            FinalVerificationError::BackendUnavailable {
                reason: "final-verification isolation setup failed",
            }
        } else {
            FinalVerificationError::Launch(source)
        }
    })?;
    let clock = SystemClock::new();
    let deadline = timeout.map(|duration| clock.now_instant() + duration);
    let timed_out = loop {
        if child
            .try_wait()
            .map_err(FinalVerificationError::Launch)?
            .is_some()
        {
            break false;
        }
        if deadline.is_some_and(|deadline| clock.now_instant() >= deadline) {
            child.kill().map_err(FinalVerificationError::Launch)?;
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child
        .wait_with_output()
        .map_err(FinalVerificationError::Launch)?;
    Ok(FinalVerificationResult {
        exit_code: output.status.code(),
        timed_out,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn isolation_setup_error() -> io::Error {
    io::Error::from_raw_os_error(ISOLATION_SETUP_ERROR)
}

/// Whether the two kernel mechanisms required by this launcher are available.
pub fn final_verification_backend_available() -> bool {
    probe_required_filesystem_policy() && probe_network_namespace()
}

fn ensure_backend_available() -> Result<(), FinalVerificationError> {
    if !probe_required_filesystem_policy() {
        return Err(FinalVerificationError::BackendUnavailable {
            reason: "Landlock ABI V3 policy is unavailable",
        });
    }
    if !probe_network_namespace() {
        return Err(FinalVerificationError::BackendUnavailable {
            reason: "network namespaces are unavailable",
        });
    }
    Ok(())
}

fn landlock_abi_supports_final_verification(abi: Option<i32>) -> bool {
    abi.is_some_and(|abi| abi >= REQUIRED_LANDLOCK_ABI)
}

/// Verify that the exact V3 filesystem access set used for final verification
/// can be created and installed. Installation happens in a disposable thread
/// because Landlock restrictions cannot be removed from that thread.
fn probe_required_filesystem_policy() -> bool {
    probe_required_filesystem_policy_with(crate::landlock_abi(), || {
        // Using `/` as the read root exercises apply_filesystem_policy's exact
        // handled-access set, device rules, ruleset creation, installation,
        // and `RulesetStatus::FullyEnforced` check without granting the probe
        // thread any new privileges.
        apply_filesystem_policy(Path::new("/"), &[], &[], &[])
    })
}

fn probe_required_filesystem_policy_with(
    abi: Option<i32>,
    install_policy: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
) -> bool {
    landlock_abi_supports_final_verification(abi)
        && matches!(std::thread::spawn(install_policy).join(), Ok(Ok(())))
}

#[derive(Debug)]
struct PreparedRequest {
    argv: Vec<String>,
    worktree: PathBuf,
    working_directory: PathBuf,
    tool_runtime: Vec<PathBuf>,
    read_only_external_mounts: Vec<PathBuf>,
    output_directories: Vec<PathBuf>,
    environment: BTreeMap<String, String>,
}

impl PreparedRequest {
    fn new(request: FinalVerificationRequest) -> Result<Self, FinalVerificationViolation> {
        if request.argv.first().is_none_or(String::is_empty)
            || request.argv.iter().any(|arg| arg.contains('\0'))
        {
            return Err(FinalVerificationViolation::InvalidArgv);
        }
        let worktree = canonical_directory("worktree", &request.worktree)?;
        let working_directory =
            canonical_directory("working_directory", &request.working_directory)?;
        if !working_directory.starts_with(&worktree) {
            return Err(
                FinalVerificationViolation::WorkingDirectoryOutsideWorktree {
                    path: working_directory,
                },
            );
        }
        let tool_runtime = request
            .tool_runtime
            .iter()
            .map(|path| canonical_directory("tool_runtime", path))
            .collect::<Result<Vec<_>, _>>()?;
        let read_only_external_mounts = request
            .read_only_external_mounts
            .iter()
            .map(|path| canonical_directory("read_only_external_mount", path))
            .collect::<Result<Vec<_>, _>>()?;

        let mut output_directories = Vec::with_capacity(request.output_directories.len());
        for output in request.output_directories {
            if output.as_os_str().is_empty()
                || output.is_absolute()
                || output.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(FinalVerificationViolation::InvalidOutputDirectory { path: output });
            }
            let path = worktree.join(&output);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(FinalVerificationViolation::OutputOnlyPreexisting { path });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(FinalVerificationViolation::InvalidOutputDirectory { path });
                }
            }
            let parent = path
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .filter(|parent| parent.starts_with(&worktree));
            if parent.is_none() {
                return Err(FinalVerificationViolation::InvalidOutputDirectory { path });
            }
            output_directories.push(path);
        }
        for (index, first) in output_directories.iter().enumerate() {
            for second in &output_directories[index + 1..] {
                if first.starts_with(second) || second.starts_with(first) {
                    return Err(FinalVerificationViolation::OverlappingOutputDirectories {
                        first: first.clone(),
                        second: second.clone(),
                    });
                }
            }
        }

        Ok(Self {
            argv: request.argv,
            worktree,
            working_directory,
            tool_runtime,
            read_only_external_mounts,
            output_directories,
            environment: request.environment,
        })
    }

    fn create_empty_output_directories(&self) -> Result<(), FinalVerificationError> {
        for path in &self.output_directories {
            std::fs::create_dir(path).map_err(|source| {
                FinalVerificationError::OutputPreparation {
                    path: path.clone(),
                    source,
                }
            })?;
        }
        Ok(())
    }
}

fn canonical_directory(
    field: &'static str,
    path: &Path,
) -> Result<PathBuf, FinalVerificationViolation> {
    let canonical =
        path.canonicalize()
            .map_err(|_| FinalVerificationViolation::InvalidDirectory {
                field,
                path: path.to_path_buf(),
            })?;
    if !canonical.is_dir() {
        return Err(FinalVerificationViolation::InvalidDirectory {
            field,
            path: canonical,
        });
    }
    Ok(canonical)
}

fn apply_filesystem_policy(
    worktree: &Path,
    tool_runtime: &[PathBuf],
    external_mounts: &[PathBuf],
    output_directories: &[PathBuf],
) -> anyhow::Result<()> {
    let abi = ABI::V3;
    let full = AccessFs::from_all(abi);
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;
    let mut ruleset = Ruleset::default()
        .handle_access(full)?
        .create()?
        .add_rule(PathBeneath::new(PathFd::new(worktree)?, read_exec))?;

    for path in tool_runtime.iter().chain(external_mounts) {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, read_exec))?;
    }
    for path in output_directories {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, full))?;
    }
    // The process needs these character devices for conventional tool startup,
    // but no other host paths are granted.
    for device in ["/dev/null", "/dev/zero", "/dev/urandom"] {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(device)?, full))?;
    }
    let status = ruleset.restrict_self()?;
    anyhow::ensure!(
        status.ruleset == RulesetStatus::FullyEnforced,
        "Landlock policy was not fully enforced"
    );
    Ok(())
}

fn close_fd(fd: RawFd) {
    unsafe { libc::close(fd) };
}
fn pipe_cloexec() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0 {
        Ok((fds[0], fds[1]))
    } else {
        Err(io::Error::last_os_error())
    }
}
fn seqpacket_pair() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } == 0
    {
        Ok((fds[0], fds[1]))
    } else {
        Err(io::Error::last_os_error())
    }
}
fn read_ready(fd: RawFd) -> bool {
    let mut byte = 0_u8;
    (unsafe { libc::read(fd, std::ptr::addr_of_mut!(byte).cast(), 1) }) == 1 && byte == 1
}
fn join_namespace_fds(user_namespace: RawFd, network_namespace: RawFd) -> io::Result<()> {
    // The network namespace is owned by this user namespace, so user must be
    // entered first for the capability check in setns(2).
    if unsafe { libc::setns(user_namespace, libc::CLONE_NEWUSER) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setns(network_namespace, libc::CLONE_NEWNET) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_namespace(pid: libc::pid_t, name: &str) -> io::Result<std::fs::File> {
    std::fs::File::open(format!("/proc/{pid}/ns/{name}"))
}
fn wait_for_pid(pid: libc::pid_t) {
    let mut status = 0;
    while unsafe { libc::waitpid(pid, &mut status, 0) } < 0
        && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
    {}
}
fn keeper_main(
    ready_fd: RawFd,
    channels: Vec<(FinalVerificationLoopbackEndpoint, RawFd, RawFd)>,
) -> ! {
    let configured = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } == 0
        && bring_loopback_up().is_ok();
    if !configured {
        unsafe {
            libc::write(ready_fd, b"\0".as_ptr().cast(), 1);
            libc::_exit(1)
        };
    }
    let mut listeners = Vec::with_capacity(channels.len());
    for (endpoint, _, child) in channels {
        match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, endpoint.port)) {
            Ok(listener) => listeners.push((listener, child)),
            Err(_) => unsafe {
                libc::write(ready_fd, b"\0".as_ptr().cast(), 1);
                libc::_exit(1)
            },
        }
    }
    unsafe {
        libc::write(ready_fd, b"\x01".as_ptr().cast(), 1);
        libc::close(ready_fd)
    };
    // A listener has exactly one inherited private channel and fixed parent port.
    for (listener, channel) in listeners {
        std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = send_fd(channel, stream.into_raw_fd());
                    }
                    Err(_) => break,
                }
            }
        });
    }
    loop {
        unsafe { libc::pause() };
    }
}
fn bring_loopback_up() -> io::Result<()> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    request.ifr_name[..2].copy_from_slice(&[b'l' as i8, b'o' as i8]);
    if unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut request) } != 0 {
        close_fd(fd);
        return Err(io::Error::last_os_error());
    }
    unsafe {
        request.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as i16;
    }
    let result = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &request) };
    let error = io::Error::last_os_error();
    close_fd(fd);
    if result == 0 { Ok(()) } else { Err(error) }
}
fn send_fd(channel: RawFd, client: RawFd) -> io::Result<()> {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: std::ptr::addr_of_mut!(byte).cast(),
        iov_len: 1,
    };
    let mut control = [0_u8; 64];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = std::ptr::addr_of_mut!(iov);
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as _) as _;
        *(libc::CMSG_DATA(header).cast::<RawFd>()) = client;
        message.msg_controllen = (*header).cmsg_len;
    }
    let sent = unsafe { libc::sendmsg(channel, &message, 0) };
    close_fd(client);
    if sent == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn receive_fd(channel: RawFd) -> io::Result<Option<RawFd>> {
    let mut byte = 0_u8;
    let mut iov = libc::iovec {
        iov_base: std::ptr::addr_of_mut!(byte).cast(),
        iov_len: 1,
    };
    let mut control = [0_u8; 64];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = std::ptr::addr_of_mut!(iov);
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received = unsafe { libc::recvmsg(channel, &mut message, libc::MSG_DONTWAIT) };
    if received == 0 {
        return Ok(None);
    }
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other("invalid broker message"));
        }
        Ok(Some(*(libc::CMSG_DATA(header).cast::<RawFd>())))
    }
}
fn broker_main(channel: RawFd, port: u16, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        match receive_fd(channel) {
            Ok(Some(client)) => {
                std::thread::spawn(move || {
                    let mut client = unsafe { TcpStream::from_raw_fd(client) };
                    let Ok(mut backend) = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                    else {
                        return;
                    };
                    let mut reverse = backend.try_clone().ok();
                    let mut client_copy = client.try_clone().ok();
                    let forward = std::thread::spawn(move || {
                        if let Some(mut c) = client_copy.take() {
                            let _ = io::copy(&mut c, &mut backend);
                            let _ = backend.shutdown(Shutdown::Write);
                        }
                    });
                    if let Some(mut b) = reverse.take() {
                        let _ = io::copy(&mut b, &mut client);
                        let _ = client.shutdown(Shutdown::Write);
                    }
                    let _ = forward.join();
                });
            }
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(_) => break,
        }
    }
    close_fd(channel);
}

fn child_exited_successfully(pid: libc::pid_t) -> bool {
    let mut status = 0;
    (unsafe { libc::waitpid(pid, &mut status, 0) >= 0 })
        && libc::WIFEXITED(status)
        && libc::WEXITSTATUS(status) == 0
}

fn probe_network_namespace() -> bool {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return false;
    }
    if pid == 0 {
        let ok = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } == 0;
        unsafe { libc::_exit(if ok { 0 } else { 1 }) };
    }
    child_exited_successfully(pid)
}

fn enter_isolated_network_namespace() -> io::Result<()> {
    let result = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn request(worktree: &Path) -> FinalVerificationRequest {
        FinalVerificationRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
            worktree: worktree.to_path_buf(),
            working_directory: worktree.to_path_buf(),
            tool_runtime: ["/bin", "/usr", "/lib", "/lib64"]
                .into_iter()
                .filter(|path| Path::new(path).is_dir())
                .map(PathBuf::from)
                .collect(),
            read_only_external_mounts: Vec::new(),
            output_directories: vec![PathBuf::from("outputs")],
            environment: BTreeMap::from([("PATH".to_string(), "/bin:/usr/bin".to_string())]),
        }
    }

    #[test]
    fn preexisting_output_content_is_a_public_typed_sandbox_violation() {
        let worktree = TempDir::new().unwrap();
        std::fs::create_dir(worktree.path().join("outputs")).unwrap();
        std::fs::write(worktree.path().join("outputs/stale"), b"old output").unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = "cat outputs/stale".into();
        let error = launch_final_verification(req).unwrap_err();
        assert!(matches!(
            error,
            FinalVerificationError::Violation(
                FinalVerificationViolation::OutputOnlyPreexisting { .. }
            )
        ));
        assert_eq!(
            std::fs::read(worktree.path().join("outputs/stale")).unwrap(),
            b"old output"
        );
    }

    #[test]
    fn output_directory_must_be_relative_and_absent() {
        let worktree = TempDir::new().unwrap();
        let mut invalid = request(worktree.path());
        invalid.output_directories = vec![PathBuf::from("../escaped")];
        assert!(matches!(
            PreparedRequest::new(invalid),
            Err(FinalVerificationViolation::InvalidOutputDirectory { .. })
        ));
    }

    #[test]
    fn unsupported_backends_fail_closed() {
        if !final_verification_backend_available() {
            let worktree = TempDir::new().unwrap();
            assert!(matches!(
                launch_final_verification(request(worktree.path())),
                Err(FinalVerificationError::BackendUnavailable { .. })
            ));
        }
    }

    #[test]
    fn final_verification_rejects_landlock_abi_v1_and_v2() {
        assert!(!landlock_abi_supports_final_verification(None));
        assert!(!landlock_abi_supports_final_verification(Some(1)));
        assert!(!landlock_abi_supports_final_verification(Some(2)));
        assert!(landlock_abi_supports_final_verification(Some(3)));
        assert!(landlock_abi_supports_final_verification(Some(4)));
    }

    #[test]
    fn required_policy_probe_installs_on_a_disposable_thread() {
        let caller = std::thread::current().id();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        assert!(probe_required_filesystem_policy_with(
            Some(REQUIRED_LANDLOCK_ABI),
            move || {
                sender.send(std::thread::current().id()).unwrap();
                Ok(())
            }
        ));
        assert_ne!(receiver.recv().unwrap(), caller);
    }

    #[test]
    fn unsupported_abi_does_not_start_a_policy_probe_thread() {
        assert!(!probe_required_filesystem_policy_with(Some(2), || {
            panic!("unsupported ABI must not install a policy")
        }));
    }

    #[test]
    fn unavailable_required_policy_does_not_create_output() {
        let worktree = TempDir::new().unwrap();
        let output = worktree.path().join("outputs");
        let error = launch_with_backend_check(request(worktree.path()), || {
            Err(FinalVerificationError::BackendUnavailable {
                reason: "injected unavailable V3 policy",
            })
        })
        .unwrap_err();

        assert!(matches!(
            error,
            FinalVerificationError::BackendUnavailable { .. }
        ));
        assert!(!output.exists());
    }

    #[test]
    fn actual_isolation_setup_failure_is_backend_unavailable() {
        let worktree = TempDir::new().unwrap();
        let error = launch_with_backend_and_setup(
            request(worktree.path()),
            || Ok(()),
            None,
            |_, _, _, _| Err(()),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            FinalVerificationError::BackendUnavailable { .. }
        ));
    }

    #[test]
    fn declared_external_and_new_output_work_when_backend_is_available() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        std::fs::write(external.path().join("input"), b"declared").unwrap();
        let mut req = request(worktree.path());
        req.read_only_external_mounts = vec![external.path().to_path_buf()];
        req.argv[2] = format!("cat {}/input > outputs/result", external.path().display());
        let result = launch_final_verification(req).unwrap();
        assert!(
            result.succeeded(),
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            std::fs::read(worktree.path().join("outputs/result")).unwrap(),
            b"declared"
        );
    }

    #[test]
    fn suppressed_undeclared_host_reads_are_non_success_results() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let host_only = TempDir::new().unwrap();
        let secret = host_only.path().join("secret");
        std::fs::write(&secret, b"must not be readable").unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = format!("cat {} >/dev/null 2>&1", secret.display());
        let result = launch_final_verification(req).unwrap();
        assert!(!result.succeeded());
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn suppressed_network_access_is_a_non_success_result() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/bash".into(),
            "-c".into(),
            "{ exec 3<>/dev/tcp/1.1.1.1/80; } 2>/dev/null".into(),
        ];
        let result = launch_final_verification(req).unwrap();
        assert!(
            !result.succeeded(),
            "network namespace allowed a TCP connection"
        );
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn child_stderr_cannot_forge_a_runtime_violation() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = "printf 'permission denied\\n' >&2; exit 9".into();
        let result = launch_final_verification(req).unwrap();
        assert_eq!(result.exit_code, Some(9));
        assert_eq!(result.stderr, b"permission denied\n");
    }

    #[test]
    fn ordinary_mode_permission_failure_is_an_ordinary_result() {
        use std::os::unix::fs::PermissionsExt;

        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let locked = worktree.path().join("locked");
        std::fs::write(&locked, b"allowed path, denied by Unix mode").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut req = request(worktree.path());
        req.argv[2] = "cat locked >/dev/null 2>&1; exit 7".into();
        let result = launch_final_verification(req).unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert!(result.stderr.is_empty());
    }

    #[test]
    fn empty_non_executable_argument_is_passed_faithfully() {
        if !final_verification_backend_available() {
            return;
        }
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "test \"$1\" = \"\"".into(),
            "--".into(),
            "".into(),
        ];
        let result = launch_final_verification(req).unwrap();
        assert!(result.succeeded());
    }

    #[test]
    fn attempt_session_proxies_only_the_cataloged_parent_loopback_port() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let target = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = target.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = target.accept().unwrap();
            let mut request = [0; 4];
            std::io::Read::read_exact(&mut stream, &mut request).unwrap();
            std::io::Write::write_all(&mut stream, &request).unwrap();
        });
        let session =
            FinalVerificationNetworkSession::create(vec![FinalVerificationLoopbackEndpoint {
                preset_id: "echo".into(),
                port,
            }])
            .unwrap();
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/bash".into(),
            "-c".into(),
            format!(
                "exec 3<>/dev/tcp/127.0.0.1/{port}; printf ping >&3; head -c 4 <&3 > outputs/result"
            ),
        ];
        let result = launch_final_verification_in_network_session_with_timeout(
            req,
            &session,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            result.succeeded(),
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            std::fs::read(worktree.path().join("outputs/result")).unwrap(),
            b"ping"
        );
        server.join().unwrap();
        // The session is the listener owner; dropping it terminates the keeper
        // and no parent-namespace listener was ever opened on this port.
        drop(session);
    }

    #[test]
    fn malformed_or_unavailable_catalog_listener_fails_closed() {
        assert!(matches!(
            FinalVerificationNetworkSession::create(vec![FinalVerificationLoopbackEndpoint {
                preset_id: String::new(),
                port: 1
            }]),
            Err(FinalVerificationError::BackendUnavailable { .. })
        ));
    }

    #[test]
    fn attempt_session_denies_an_absent_loopback_port() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let catalog_target = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let catalog_port = catalog_target.local_addr().unwrap().port();
        let absent_port = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let session =
            FinalVerificationNetworkSession::create(vec![FinalVerificationLoopbackEndpoint {
                preset_id: "cataloged".into(),
                port: catalog_port,
            }])
            .unwrap();
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/bash".into(),
            "-c".into(),
            format!("{{ exec 3<>/dev/tcp/127.0.0.1/{absent_port}; }} 2>/dev/null"),
        ];
        let result = launch_final_verification_in_network_session_with_timeout(
            req,
            &session,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(
            !result.succeeded(),
            "uncataloged loopback port was reachable"
        );
        drop(session);
        drop(catalog_target);
    }

    #[test]
    fn attempt_session_denies_non_loopback_destinations() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let session = FinalVerificationNetworkSession::create(vec![]).unwrap();
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/bash".into(),
            "-c".into(),
            "{ exec 3<>/dev/tcp/1.1.1.1/80; } 2>/dev/null".into(),
        ];
        let result = launch_final_verification_in_network_session_with_timeout(
            req,
            &session,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(
            !result.succeeded(),
            "attempt namespace has a non-loopback route"
        );
        drop(session);
    }

    #[test]
    fn attempt_session_does_not_inherit_broker_control_descriptors() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let target = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = target.local_addr().unwrap().port();
        let session =
            FinalVerificationNetworkSession::create(vec![FinalVerificationLoopbackEndpoint {
                preset_id: "cataloged".into(),
                port,
            }])
            .unwrap();
        let control_fds = session
            .broker_fd_numbers
            .iter()
            .chain(
                [
                    session.user_namespace.as_raw_fd(),
                    session.network_namespace.as_raw_fd(),
                ]
                .iter(),
            )
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec![
            "/bin/bash".into(),
            "-c".into(),
            format!("for fd in {control_fds}; do test ! -e /proc/self/fd/$fd || exit 1; done"),
        ];
        let result = launch_final_verification_in_network_session_with_timeout(
            req,
            &session,
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(result.succeeded(), "control descriptors survived exec");
        drop(session);
        drop(target);
    }

    #[test]
    fn attempt_session_reaps_keeper_after_command_cancellation_and_drop() {
        if !final_verification_backend_available() || !Path::new("/bin/bash").is_file() {
            return;
        }
        let session = FinalVerificationNetworkSession::create(vec![]).unwrap();
        let keeper_pid = session.keeper_pid;
        let worktree = TempDir::new().unwrap();
        let mut req = request(worktree.path());
        req.argv = vec!["/bin/bash".into(), "-c".into(), "sleep 5".into()];
        let result = launch_final_verification_in_network_session_with_timeout(
            req,
            &session,
            Duration::from_millis(25),
        )
        .unwrap();
        assert!(
            result.timed_out,
            "command cancellation did not enforce timeout"
        );
        drop(session);
        assert_eq!(unsafe { libc::kill(keeper_pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}
