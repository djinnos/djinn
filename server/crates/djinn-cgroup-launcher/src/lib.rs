//! Fail-closed, cgroup-v2 leaf lifecycle primitives.
//!
//! This crate intentionally has no lease transport or process protocol.  The
//! caller supplies an invocation-local fence and an injectable spawn syscall;
//! this boundary only creates and controls one direct child of a validated
//! delegated cgroup root.

use std::{collections::BTreeSet, ffi::CString, fs, io, os::fd::RawFd, path::Path};

use thiserror::Error;

pub mod bootstrap;
pub mod broker;
pub mod child;
pub mod env;
pub mod git_trust;
pub mod spawn;
pub mod transport;

pub use env::{is_allowed_environment_entry, is_allowed_environment_key};
pub use spawn::{DenySpawn, NativeCgroupSpawn};

const DEFAULT_PERIOD_US: u64 = 100_000;

/// CPU quota an invocation leaf runs at before a lease lifts it.
///
/// Deliberately small: an unleased command is throttled hard enough that a
/// genuinely CPU-hungry one is detectable within a few scheduling periods, and
/// cheap enough that a light one is unaffected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnleasedQuota(u16);

impl UnleasedQuota {
    pub const DEFAULT_MILLICORES: u16 = 250;

    pub fn new(millicores: u16) -> Result<Self, Error> {
        if !(50..=1000).contains(&millicores) {
            return Err(Error::InvalidQuota(millicores));
        }
        Ok(Self(millicores))
    }

    pub fn millicores(self) -> u16 {
        self.0
    }

    fn cpu_max(self) -> String {
        cpu_max_for(u32::from(self.0))
    }
}

impl Default for UnleasedQuota {
    fn default() -> Self {
        Self(Self::DEFAULT_MILLICORES)
    }
}

/// CPU quota an invocation leaf is lifted to when a lease is granted.
///
/// # Why a lift is a quota and not `max` (task 7deu, defect 1)
///
/// The lift used to write `max`, i.e. remove the leaf's quota entirely, which
/// was harmless only because an ancestor still clamped it: the launcher
/// container carried a 250m CPU limit, and under `nsdelegate` the delegated root
/// IS that container's cgroup. Every build was therefore permanently capped at
/// 250m no matter what the leaf said — measured at 0.25 core against a leaf set
/// to four — and `nr_throttled` read 0 in the leaf because the throttling
/// happened at the parent, which made throttle-based heavy detection
/// structurally blind.
///
/// Removing the container's CPU limit fixes that, but it also removes the only
/// remaining ceiling. So the ceiling moves to where the work actually is: the
/// lease grants an explicit quota equal to the pod's own CPU budget. One lifted
/// build gets exactly the budget the pod declares, and N concurrent lifted
/// builds are bounded by N × budget — which is what makes the admission cap a
/// real semaphore instead of an advisory number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeasedQuota(u32);

impl LeasedQuota {
    /// Four cores: the task-run pod's declared CPU limit in production.
    pub const DEFAULT_MILLICORES: u32 = 4_000;
    /// Below this a "lease" would not be a lift at all.
    pub const MIN_MILLICORES: u32 = 1_000;
    /// A single leaf may never be granted more than one large node's worth.
    pub const MAX_MILLICORES: u32 = 64_000;

    pub fn new(millicores: u32) -> Result<Self, Error> {
        if !(Self::MIN_MILLICORES..=Self::MAX_MILLICORES).contains(&millicores) {
            return Err(Error::InvalidLeasedQuota(millicores));
        }
        Ok(Self(millicores))
    }

    pub fn millicores(self) -> u32 {
        self.0
    }

    fn cpu_max(self) -> String {
        cpu_max_for(self.0)
    }
}

impl Default for LeasedQuota {
    fn default() -> Self {
        Self(Self::DEFAULT_MILLICORES)
    }
}

/// `cpu.max` line for `millicores` over the default 100ms period.
fn cpu_max_for(millicores: u32) -> String {
    format!(
        "{} {DEFAULT_PERIOD_US}",
        u64::from(millicores) * DEFAULT_PERIOD_US / 1000
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherConfig {
    pub unleased_quota: UnleasedQuota,
    pub leased_quota: LeasedQuota,
    pub expected_uid: u32,
}

impl LauncherConfig {
    pub fn new(
        unleased_millicores: Option<u16>,
        leased_millicores: Option<u32>,
        expected_uid: u32,
    ) -> Result<Self, Error> {
        let unleased =
            UnleasedQuota::new(unleased_millicores.unwrap_or(UnleasedQuota::DEFAULT_MILLICORES))?;
        let leased =
            LeasedQuota::new(leased_millicores.unwrap_or(LeasedQuota::DEFAULT_MILLICORES))?;
        if u32::from(unleased.millicores()) >= leased.millicores() {
            return Err(Error::LeaseIsNotALift {
                unleased: unleased.millicores(),
                leased: leased.millicores(),
            });
        }
        Ok(Self {
            unleased_quota: unleased,
            leased_quota: leased,
            expected_uid,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CgroupMode {
    V1,
    V2,
    Hybrid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Readiness {
    pub mode: CgroupMode,
    pub root_writable: bool,
    pub owner_uid: u32,
    /// Controllers enabled for children at the delegated root.  It must be
    /// exactly `cpu`; allowing unrelated controllers gives the launcher a
    /// broader delegation than its contract.
    pub delegated_controllers: BTreeSet<String>,
}

impl Readiness {
    pub fn validate(&self, expected_uid: u32) -> Result<(), Error> {
        if self.mode != CgroupMode::V2 {
            return Err(Error::NotCgroupV2);
        }
        if !self.root_writable {
            return Err(Error::ReadOnlyDelegation);
        }
        if self.owner_uid != expected_uid {
            return Err(Error::IncompatibleOwnership {
                expected: expected_uid,
                actual: self.owner_uid,
            });
        }
        if self.delegated_controllers != BTreeSet::from(["cpu".to_owned()]) {
            return Err(Error::OverbroadOrMissingCpuDelegation);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub id: String,
    pub fence: u64,
}

/// Data-only request accepted by the privileged broker. It is deliberately
/// bounded and has no file-descriptor inheritance surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub environment: Vec<(String, String)>,
}

impl CommandSpec {
    pub const MAX_ARGUMENTS: usize = 128;
    /// A task-run Pod renders a real build environment (cargo target dir, mold
    /// flags, rustup home, sccache, the language toolchains) plus the launcher's
    /// own pins. The former six-entry ceiling could not carry it.
    pub const MAX_ENVIRONMENT: usize = 96;
    pub const MAX_BYTES: usize = 32 * 1024;

    pub fn validate(&self) -> Result<(), Error> {
        if !safe_command_path(&self.program, false)
            || !safe_command_path(&self.cwd, true)
            || self.argv.len() > Self::MAX_ARGUMENTS
            || self.environment.len() > Self::MAX_ENVIRONMENT
        {
            return Err(Error::InvalidCommand);
        }
        let mut bytes = self.program.len() + self.cwd.len();
        for value in &self.argv {
            if value.contains('\0') {
                return Err(Error::InvalidCommand);
            }
            bytes = bytes.saturating_add(value.len());
        }
        for (key, value) in &self.environment {
            // `execve` receives each entry as `key=value`; accepting either
            // separator or NUL in the key would make that representation
            // malformed or change the key observed by the child.
            //
            // The predicate is over the whole ENTRY, not the key: one forwarded
            // name (`GIT_CONFIG_SYSTEM`) is a pointer to a configuration file,
            // and admitting it by name would let a caller aim the child's git
            // config at a file it wrote. See `env::is_allowed_environment_entry`.
            if !is_allowed_environment_entry(key, value)
                || key.contains(['\0', '='])
                || value.contains('\0')
            {
                return Err(Error::InvalidCommand);
            }
            bytes = bytes.saturating_add(key.len() + value.len());
        }
        (bytes <= Self::MAX_BYTES)
            .then_some(())
            .ok_or(Error::InvalidCommand)
    }
}

fn safe_command_path(path: &str, cwd: bool) -> bool {
    !path.contains('\0')
        && !path.contains("//")
        && !path.split('/').any(|part| part == "..")
        && if cwd {
            path == "/workspace" || path.starts_with("/workspace/")
        } else {
            path.starts_with("/bin/")
                || path.starts_with("/usr/bin/")
                || path.starts_with("/workspace/")
        }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildProcess {
    pub pid: i32,
    pub stdout: RawFd,
    pub stderr: RawFd,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Leaf {
    name: String,
    fd: RawFd,
    invocation: Invocation,
    lifted: bool,
    terminal: bool,
}

impl Leaf {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn cgroup_fd(&self) -> RawFd {
        self.fd
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct CpuStat {
    pub usage_usec: u64,
    pub user_usec: u64,
    pub system_usec: u64,
    pub nr_periods: u64,
    pub nr_throttled: u64,
    pub throttled_usec: u64,
}

pub trait CgroupFs {
    fn readiness(&self) -> Result<Readiness, Error>;
    /// Create and open a leaf with no symlink traversal.  The implementation
    /// must reject names that are not direct children of the delegated root.
    fn create_direct_child(&mut self, name: &str) -> Result<RawFd, Error>;
    fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), Error>;
    fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, Error>;
    fn remove_leaf(&mut self, fd: RawFd, name: &str) -> Result<(), Error>;
}

/// The only child-spawn seam.
///
/// Implementations must place the child inside `target_cgroup_fd` **and verify
/// the placement** before the child is allowed to `execve`; see
/// [`spawn::NativeCgroupSpawn`] for the production `fork` + pipe-handshake +
/// `cgroup.procs` implementation and why it replaced `clone3`.
pub trait SpawnIntoCgroup {
    fn spawn_into_cgroup(
        &mut self,
        target_cgroup_fd: RawFd,
        invocation: &Invocation,
        command: &CommandSpec,
    ) -> Result<ChildProcess, Error>;
}

pub struct Launcher<F, S> {
    fs: F,
    syscall: S,
    config: LauncherConfig,
}

impl<F: CgroupFs, S: SpawnIntoCgroup> Launcher<F, S> {
    pub fn new(fs: F, syscall: S, config: LauncherConfig) -> Result<Self, Error> {
        fs.readiness()?.validate(config.expected_uid)?;
        Ok(Self {
            fs,
            syscall,
            config,
        })
    }

    /// The quota a lease lifts an invocation leaf to.
    pub fn leased_quota(&self) -> LeasedQuota {
        self.config.leased_quota
    }

    pub fn create_command(
        &mut self,
        name: &str,
        invocation: Invocation,
        command: &CommandSpec,
    ) -> Result<(Leaf, ChildProcess), Error> {
        validate_leaf_name(name)?;
        command.validate()?;

        // Pin build parallelism from the LEASED quota, not the quota the child
        // is born at. `available_parallelism()` is sampled once at process
        // start and never re-read, so a pin taken from the 250m unleased lane
        // would leave every lifted build stuck at `-j1`. See
        // `env::parallelism_pins` for the full argument.
        let mut command = command.clone();
        env::apply_parallelism_pins(
            &mut command.environment,
            self.config.leased_quota.millicores(),
        );
        // Give the child a git trust anchor the LAUNCHER owns. Without it every
        // brokered `git` command in the worker-created workspace dies "dubious
        // ownership" — the workspace is owned by the worker uid and the child is
        // a different one. It is derived here, never accepted from the caller,
        // for the same reason the parallelism pins are: only this side of the
        // boundary knows a value that is safe to use. See `crate::git_trust`.
        env::apply_git_trust_anchor(&mut command.environment, git_trust::anchor_path()?);
        command.validate()?;

        let fd = self.fs.create_direct_child(name)?;
        self.fs
            .write_leaf(fd, "cpu.max", &self.config.unleased_quota.cpu_max())?;
        // Spawn is deliberately last: all readiness and leaf setup failures
        // occur before the child can execute.
        let child = match self.syscall.spawn_into_cgroup(fd, &invocation, &command) {
            Ok(child) => child,
            Err(error) => {
                // The spawn seam stands the child down before `execve` on every
                // failure path, so this direct child is necessarily empty. Do
                // not leak a delegated cgroup on refusal.
                let _ = self.fs.remove_leaf(fd, name);
                return Err(error);
            }
        };
        Ok((
            Leaf {
                name: name.to_owned(),
                fd,
                invocation,
                lifted: false,
                terminal: false,
            },
            child,
        ))
    }

    pub fn sample(&mut self, leaf: &Leaf) -> Result<CpuStat, Error> {
        parse_cpu_stat(&self.fs.read_leaf(leaf.fd, "cpu.stat")?)
    }

    pub fn fenced_lift(&mut self, leaf: &mut Leaf, fence: u64) -> Result<(), Error> {
        if leaf.terminal {
            return Err(Error::TerminalIntent);
        }
        if leaf.lifted {
            return Err(Error::LiftAlreadyApplied);
        }
        if leaf.invocation.fence != fence {
            return Err(Error::FenceMismatch);
        }
        self.fs
            .write_leaf(leaf.fd, "cpu.max", &self.config.leased_quota.cpu_max())?;
        leaf.lifted = true;
        Ok(())
    }

    /// Mark terminal intent before cgroup-wide kill so a concurrent/replayed
    /// grant can never lift it afterwards.
    pub fn kill(&mut self, leaf: &mut Leaf) -> Result<(), Error> {
        leaf.terminal = true;
        self.fs.write_leaf(leaf.fd, "cgroup.kill", "1")
    }

    pub fn wait_empty(&mut self, leaf: &Leaf) -> Result<(), Error> {
        let events = self.fs.read_leaf(leaf.fd, "cgroup.events")?;
        if populated_zero(&events)? {
            Ok(())
        } else {
            Err(Error::StillPopulated)
        }
    }

    pub fn remove(&mut self, leaf: &Leaf) -> Result<(), Error> {
        self.wait_empty(leaf)?;
        self.fs.remove_leaf(leaf.fd, &leaf.name)
    }

    pub fn into_parts(self) -> (F, S) {
        (self.fs, self.syscall)
    }
}

fn validate_leaf_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(Error::UnsafeLeafName);
    }
    Ok(())
}

fn parse_cpu_stat(input: &str) -> Result<CpuStat, Error> {
    let mut result = CpuStat::default();
    for line in input.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(key) = fields.next() else { continue };
        let Some(raw) = fields.next() else {
            return Err(Error::InvalidCpuStat);
        };
        if fields.next().is_some() {
            return Err(Error::InvalidCpuStat);
        }
        let value = raw.parse().map_err(|_| Error::InvalidCpuStat)?;
        match key {
            "usage_usec" => result.usage_usec = value,
            "user_usec" => result.user_usec = value,
            "system_usec" => result.system_usec = value,
            "nr_periods" => result.nr_periods = value,
            "nr_throttled" => result.nr_throttled = value,
            "throttled_usec" => result.throttled_usec = value,
            _ => {}
        }
    }
    Ok(result)
}

fn populated_zero(input: &str) -> Result<bool, Error> {
    for line in input.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() == Some("populated") {
            return match (fields.next(), fields.next()) {
                (Some("0"), None) => Ok(true),
                (Some("1"), None) => Ok(false),
                _ => Err(Error::InvalidEvents),
            };
        }
    }
    Err(Error::InvalidEvents)
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("unleased CPU quota {0}m is outside 50m..=1000m")]
    InvalidQuota(u16),
    #[error("leased CPU quota {0}m is outside 1000m..=64000m")]
    InvalidLeasedQuota(u32),
    #[error(
        "the leased quota {leased}m does not lift the unleased quota {unleased}m; a lease that \
         does not raise the ceiling is not a lease"
    )]
    LeaseIsNotALift { unleased: u16, leased: u32 },
    #[error("cgroup v2 is required (v1 and hybrid mounts are unsupported)")]
    NotCgroupV2,
    #[error(
        "delegated cgroup root {path} is not a cgroup2 filesystem (statfs f_type {actual:#x}, \
         expected {expected:#x}); nothing mounted a delegated cgroup v2 subtree there"
    )]
    DelegatedRootIsNotCgroupFs {
        path: String,
        actual: i64,
        expected: i64,
    },
    #[error(
        "could not mount a private cgroup2 filesystem at {path} (errno {errno}); the launcher \
         establishes its own delegated root and needs CAP_SYS_ADMIN plus a writable mountpoint \
         to do so"
    )]
    CgroupMountFailed { path: String, errno: i32 },
    #[error("could not {detail} on the delegated cgroup root (errno {errno})")]
    CgroupDelegationFailed { detail: &'static str, errno: i32 },
    #[error(
        "could not drop the bootstrap capabilities after establishing the delegated cgroup root \
         (errno {errno}); refusing to serve while CAP_SYS_ADMIN is still held"
    )]
    CapabilityDropFailed { errno: i32 },
    #[error("could not place child {pid} into the invocation cgroup (errno {errno})")]
    CgroupPlacementFailed { pid: i32, errno: i32 },
    #[error(
        "child {pid} is not a member of the invocation cgroup after placement; its CPU would not \
         be governed by the leaf quota, so it was stood down before exec"
    )]
    ChildNotInInvocationCgroup { pid: i32 },
    #[error("delegated cgroup root is read-only")]
    ReadOnlyDelegation,
    #[error("delegated cgroup owner {actual} differs from launcher uid {expected}")]
    IncompatibleOwnership { expected: u32, actual: u32 },
    #[error("delegated root must enable exactly the cpu controller")]
    OverbroadOrMissingCpuDelegation,
    #[error("leaf name is not a safe direct child")]
    UnsafeLeafName,
    #[error("the child-spawn seam refused to start a process")]
    SpawnDenied,
    #[error("command request is malformed, over-budget, or outside the broker allow-list")]
    InvalidCommand,
    #[error(
        "could not establish the git trust anchor the child needs; without it every brokered \
         git command in the worker-owned workspace fails \"detected dubious ownership\". The \
         launcher needs a writable temp directory it alone owns (see git_trust)"
    )]
    GitTrustAnchorUnavailable,
    #[error("child process operation failed")]
    InvalidChild,
    #[error("fencing value does not match this invocation")]
    FenceMismatch,
    #[error("lift was already applied")]
    LiftAlreadyApplied,
    #[error("terminal intent forbids lift")]
    TerminalIntent,
    #[error("cgroup still has descendants")]
    StillPopulated,
    #[error("invalid cpu.stat")]
    InvalidCpuStat,
    #[error("invalid cgroup.events")]
    InvalidEvents,
    #[error("worker identity or non-dumpability check failed")]
    InvalidWorker,
    #[error("unix peer did not match the authenticated worker")]
    UnauthenticatedPeer,
    #[error("worker-private launcher credential did not match")]
    InvalidCredential,
    #[error("control is not bound to the active launcher invocation")]
    InvalidInvocationBinding,
    #[error("control nonce was stale, forged, or replayed")]
    InvalidNonce,
    #[error("control does not match the active invocation state")]
    InvalidControl,
    #[error("invalid or oversized launcher transport frame")]
    InvalidTransportFrame,
    #[error("broker socket path is unsafe or already owned by another process")]
    UnsafeSocketPath,
    #[error("child inherited a protected broker descriptor or mount")]
    ChildIsolationViolation,
    #[error("child preparation syscall failed: {0}")]
    ChildPreparation(&'static str),
    #[error("restricted seccomp is unavailable")]
    SeccompUnavailable,
    /// A named filesystem/ownership step that failed, carrying the operation,
    /// the path it acted on, and the errno.
    ///
    /// The bare [`Io`](Error::Io) below renders as "filesystem operation failed:
    /// Operation not permitted (os error 1)", which names neither the syscall
    /// nor the path. That message cost a production rollout: it was the only
    /// evidence that `chown(2)` on the broker socket was failing for want of
    /// `CAP_CHOWN`, and it was not enough to act on. Any step whose failure is a
    /// readiness/permission question should use this instead.
    #[error("{operation} failed on {path} (errno {errno})")]
    SocketSetupFailed {
        operation: &'static str,
        path: String,
        errno: i32,
    },
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
}

/// Linux no-follow implementation for the filesystem seam. It only ever uses
/// `openat` relative to its already opened delegated root or leaf descriptor.
pub struct NativeCgroupFs {
    root: RawFd,
    readiness: Readiness,
}

impl NativeCgroupFs {
    pub fn open(root: impl AsRef<Path>, expected_uid: u32) -> Result<Self, Error> {
        let root_path = root.as_ref().to_path_buf();
        let metadata = fs::symlink_metadata(&root_path)?;
        if metadata.file_type().is_symlink() {
            return Err(Error::UnsafeLeafName);
        }
        #[cfg(unix)]
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        #[cfg(unix)]
        let owner_uid = metadata.uid();
        #[cfg(unix)]
        let root_writable = metadata.permissions().mode() & 0o222 != 0
            && metadata.permissions().mode() & 0o022 == 0;
        // Prove the delegated root IS a cgroup2 tree BEFORE reading any control
        // file. Without this, a root that is any other filesystem (an emptyDir,
        // a tmpfs, a plain directory) surfaces only as a bare `ENOENT` from the
        // `cgroup.subtree_control` read below — an opaque `Io` error that says
        // nothing about the delegation actually being absent. Task grkq: that
        // exact opacity is what turned "nothing mounts a cgroup2 tree here" into
        // an unexplained sidecar CrashLoopBackOff.
        assert_cgroup2_filesystem(&root_path)?;
        let controllers = fs::read_to_string(root_path.join("cgroup.subtree_control"))?
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect();
        let mode = inspect_cgroup_mode()?;
        let readiness = Readiness {
            mode,
            root_writable,
            owner_uid,
            delegated_controllers: controllers,
        };
        readiness.validate(expected_uid)?;
        let root = open_dir(root_path.as_os_str())?;
        Ok(Self { root, readiness })
    }
}

impl Drop for NativeCgroupFs {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.root);
        }
    }
}

impl CgroupFs for NativeCgroupFs {
    fn readiness(&self) -> Result<Readiness, Error> {
        Ok(self.readiness.clone())
    }
    fn create_direct_child(&mut self, name: &str) -> Result<RawFd, Error> {
        validate_leaf_name(name)?;
        let name = CString::new(name).map_err(|_| Error::UnsafeLeafName)?;
        let rc = unsafe { libc::mkdirat(self.root, name.as_ptr(), 0o700) };
        if rc != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let fd = unsafe {
            libc::openat(
                self.root,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(fd)
    }
    fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), Error> {
        let file = safe_control_file(file)?;
        let target = unsafe {
            libc::openat(
                fd,
                file.as_ptr(),
                libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if target < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let bytes = value.as_bytes();
        let written = unsafe { libc::write(target, bytes.as_ptr().cast(), bytes.len()) };
        unsafe {
            libc::close(target);
        }
        if written != bytes.len() as isize {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
    fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, Error> {
        let file = safe_control_file(file)?;
        let target = unsafe {
            libc::openat(
                fd,
                file.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if target < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = unsafe { libc::read(target, chunk.as_mut_ptr().cast(), chunk.len()) };
            if count < 0 {
                unsafe {
                    libc::close(target);
                }
                return Err(io::Error::last_os_error().into());
            }
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count as usize]);
        }
        unsafe {
            libc::close(target);
        }
        String::from_utf8(bytes).map_err(|_| Error::InvalidCpuStat)
    }
    fn remove_leaf(&mut self, fd: RawFd, name: &str) -> Result<(), Error> {
        unsafe {
            libc::close(fd);
        }
        let name = CString::new(name).map_err(|_| Error::UnsafeLeafName)?;
        if unsafe { libc::unlinkat(self.root, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

/// `statfs.f_type` of a cgroup v2 hierarchy (`CGROUP2_SUPER_MAGIC`, see
/// `include/uapi/linux/magic.h`). Hard-coded rather than taken from `libc`
/// because its type differs per libc target (`__fsword_t` vs `c_ulong`).
pub const CGROUP2_SUPER_MAGIC: i64 = 0x6367_7270;

/// Is `path` a cgroup v2 filesystem?
///
/// Used both by the readiness contract and by [`bootstrap`], which needs to know
/// whether a previous launcher instance already established the mount.
pub fn is_cgroup2_filesystem(path: &Path) -> Result<bool, Error> {
    Ok(statfs_type(path)? == CGROUP2_SUPER_MAGIC)
}

/// Fail closed unless `path` really is a cgroup v2 filesystem.
///
/// This is the first readiness check, deliberately ahead of every control-file
/// read: a delegated root that is not a cgroup2 mount can never satisfy any
/// later check, and saying so by name is the difference between a diagnosable
/// startup failure and an opaque `ENOENT`.
fn assert_cgroup2_filesystem(path: &Path) -> Result<(), Error> {
    let actual = statfs_type(path)?;
    if actual != CGROUP2_SUPER_MAGIC {
        return Err(Error::DelegatedRootIsNotCgroupFs {
            path: path.display().to_string(),
            actual,
            expected: CGROUP2_SUPER_MAGIC,
        });
    }
    Ok(())
}

fn statfs_type(path: &Path) -> Result<i64, Error> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::UnsafeLeafName)?;
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c_path` is a NUL-terminated path and `buf` is a live, correctly
    // sized `statfs` allocation; `statfs` only writes through it on success.
    if unsafe { libc::statfs(c_path.as_ptr(), buf.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `statfs` returned 0, so it initialized `buf`.
    //
    // The cast is load-bearing even where it is a no-op: `statfs.f_type` is
    // `__fsword_t` (i64) on 64-bit glibc but `c_ulong` on musl and `i32` on
    // 32-bit targets, so normalizing to `i64` is what makes this compile
    // everywhere. Clippy only sees the target it is running on.
    #[allow(clippy::unnecessary_cast)]
    Ok(unsafe { buf.assume_init() }.f_type as i64)
}

fn safe_control_file(file: &str) -> Result<CString, Error> {
    if file.contains('/') || file.contains('\0') {
        return Err(Error::UnsafeLeafName);
    }
    CString::new(file).map_err(|_| Error::UnsafeLeafName)
}
fn open_dir(path: &std::ffi::OsStr) -> Result<RawFd, Error> {
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_bytes()).map_err(|_| Error::UnsafeLeafName)?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(fd)
    }
}
fn inspect_cgroup_mode() -> Result<CgroupMode, Error> {
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    let has_v1 = mounts.lines().any(|line| {
        line.split(" - ")
            .nth(1)
            .is_some_and(|tail| tail.starts_with("cgroup "))
    });
    let has_v2 = mounts.lines().any(|line| {
        line.split(" - ")
            .nth(1)
            .is_some_and(|tail| tail.starts_with("cgroup2 "))
    });
    Ok(match (has_v1, has_v2) {
        (false, true) => CgroupMode::V2,
        (true, true) => CgroupMode::Hybrid,
        _ => CgroupMode::V1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, path::PathBuf};

    #[derive(Default)]
    struct FakeFs {
        readiness: Option<Readiness>,
        files: HashMap<(RawFd, String), String>,
        created: Vec<String>,
        removed: Vec<String>,
        next: i32,
    }
    impl FakeFs {
        fn ready() -> Self {
            Self {
                readiness: Some(Readiness {
                    mode: CgroupMode::V2,
                    root_writable: true,
                    owner_uid: 7,
                    delegated_controllers: BTreeSet::from(["cpu".into()]),
                }),
                next: 10,
                ..Self::default()
            }
        }
    }
    impl CgroupFs for FakeFs {
        fn readiness(&self) -> Result<Readiness, Error> {
            Ok(self.readiness.clone().unwrap())
        }
        fn create_direct_child(&mut self, name: &str) -> Result<RawFd, Error> {
            self.created.push(name.into());
            self.next += 1;
            Ok(self.next)
        }
        fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), Error> {
            self.files.insert((fd, file.into()), value.into());
            Ok(())
        }
        fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, Error> {
            Ok(self
                .files
                .get(&(fd, file.into()))
                .cloned()
                .unwrap_or_default())
        }
        fn remove_leaf(&mut self, _: RawFd, name: &str) -> Result<(), Error> {
            self.removed.push(name.into());
            Ok(())
        }
    }
    #[derive(Default)]
    struct FakeSpawn {
        attempts: Vec<(RawFd, Invocation)>,
        exec_calls: Vec<(RawFd, Invocation)>,
        environments: Vec<Vec<(String, String)>>,
        deny: bool,
    }
    impl SpawnIntoCgroup for FakeSpawn {
        fn spawn_into_cgroup(
            &mut self,
            fd: RawFd,
            inv: &Invocation,
            command: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            self.attempts.push((fd, inv.clone()));
            if self.deny {
                return Err(Error::SpawnDenied);
            }
            self.exec_calls.push((fd, inv.clone()));
            self.environments.push(command.environment.clone());
            Ok(ChildProcess {
                pid: 1,
                stdout: -1,
                stderr: -1,
            })
        }
    }
    fn config() -> LauncherConfig {
        LauncherConfig::new(None, None, 7).unwrap()
    }
    fn launcher() -> Launcher<FakeFs, FakeSpawn> {
        Launcher::new(FakeFs::ready(), FakeSpawn::default(), config()).unwrap()
    }
    fn invocation() -> Invocation {
        Invocation {
            id: "run-1".into(),
            fence: 99,
        }
    }
    fn command() -> CommandSpec {
        CommandSpec {
            program: "/bin/true".into(),
            argv: vec![],
            cwd: "/workspace".into(),
            environment: vec![],
        }
    }
    fn create(l: &mut Launcher<FakeFs, FakeSpawn>, name: &str) -> Leaf {
        l.create_command(name, invocation(), &command()).unwrap().0
    }

    #[test]
    fn quota_configuration_has_a_safe_default_and_closed_bounds() {
        assert_eq!(UnleasedQuota::default().millicores(), 250);
        assert_eq!(config().unleased_quota.millicores(), 250);
        assert_eq!(config().leased_quota.millicores(), 4_000);
        assert_eq!(UnleasedQuota::new(50).unwrap().millicores(), 50);
        assert_eq!(UnleasedQuota::new(1000).unwrap().millicores(), 1000);
        assert!(matches!(
            UnleasedQuota::new(49),
            Err(Error::InvalidQuota(49))
        ));
        assert!(matches!(
            UnleasedQuota::new(1001),
            Err(Error::InvalidQuota(1001))
        ));
        assert!(matches!(
            LeasedQuota::new(999),
            Err(Error::InvalidLeasedQuota(999))
        ));
        assert!(matches!(
            LeasedQuota::new(64_001),
            Err(Error::InvalidLeasedQuota(64_001))
        ));
    }

    /// A "lease" that does not raise the ceiling is a configuration error, not
    /// a quiet no-op: it would leave every detected-heavy build throttled while
    /// telemetry reported a successful lift.
    #[test]
    fn a_lease_that_does_not_lift_is_rejected() {
        assert!(matches!(
            LauncherConfig::new(Some(1000), Some(1000), 0),
            Err(Error::LeaseIsNotALift {
                unleased: 1000,
                leased: 1000
            })
        ));
        assert!(LauncherConfig::new(Some(250), Some(1000), 0).is_ok());
    }

    /// The unleased quota throttles and the lease lifts to the pod budget — as
    /// `cpu.max` lines, since that is what the kernel reads.
    #[test]
    fn quotas_render_as_cpu_max_lines_over_the_default_period() {
        assert_eq!(UnleasedQuota::default().cpu_max(), "25000 100000");
        assert_eq!(LeasedQuota::default().cpu_max(), "400000 100000");
        assert_eq!(
            LeasedQuota::new(12_000).unwrap().cpu_max(),
            "1200000 100000"
        );
    }

    #[test]
    fn fresh_invocation_is_passed_with_leaf_fd() {
        let mut l = launcher();
        let leaf = create(&mut l, "invocation-1");
        let (_, spawn) = l.into_parts();
        assert_eq!(spawn.exec_calls, vec![(leaf.fd, invocation())]);
    }

    /// The child must be born already knowing the parallelism it will be able
    /// to use, because nothing re-reads it after the lift.
    #[test]
    fn the_child_environment_carries_pins_derived_from_the_leased_quota() {
        let mut l = Launcher::new(
            FakeFs::ready(),
            FakeSpawn::default(),
            LauncherConfig::new(Some(250), Some(4_000), 7).unwrap(),
        )
        .unwrap();
        create(&mut l, "pinned");
        let (_, spawn) = l.into_parts();
        let environment = &spawn.environments[0];
        let value = |key: &str| {
            environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
                .unwrap_or_else(|| panic!("{key} must be pinned on the child"))
        };
        assert_eq!(value("CARGO_BUILD_JOBS"), "4");
        assert_eq!(value("NEXTEST_TEST_THREADS"), "4");
        assert_eq!(value("MAKEFLAGS"), "-j4");
        assert_eq!(value("GOMAXPROCS"), "4");
    }

    /// A stale pod-level pin must not win over the launcher's derived one: the
    /// pod cannot know the quota the leaf will actually run at.
    #[test]
    fn a_caller_supplied_pin_is_overridden_by_the_launcher() {
        let mut l = launcher();
        let mut spec = command();
        spec.environment = vec![("CARGO_BUILD_JOBS".into(), "12".into())];
        l.create_command("override", invocation(), &spec).unwrap();
        let (_, spawn) = l.into_parts();
        assert_eq!(
            spawn.environments[0]
                .iter()
                .filter(|(key, _)| key == "CARGO_BUILD_JOBS")
                .collect::<Vec<_>>(),
            vec![&("CARGO_BUILD_JOBS".to_string(), "4".to_string())]
        );
    }

    /// Every child is born with the launcher's own git trust anchor and no
    /// other git configuration at all.
    #[test]
    fn the_child_environment_carries_the_launchers_git_trust_anchor() {
        let mut l = launcher();
        create(&mut l, "trusted");
        let (_, spawn) = l.into_parts();
        let anchor = git_trust::anchor_path()
            .expect("anchor")
            .display()
            .to_string();
        let git_entries: Vec<_> = spawn.environments[0]
            .iter()
            .filter(|(key, _)| key.starts_with("GIT"))
            .collect();
        assert_eq!(
            git_entries,
            vec![&(env::GIT_TRUST_ANCHOR_KEY.to_string(), anchor)],
            "exactly one git variable reaches the child, and it is the launcher's anchor"
        );
    }

    /// A caller aiming the child's SYSTEM git config at a file it controls is
    /// arbitrary command execution (`core.sshCommand`). It is refused outright
    /// rather than quietly corrected, and the refusal happens before any leaf or
    /// child exists.
    #[test]
    fn a_caller_cannot_aim_the_childs_git_config_at_its_own_file() {
        for (key, value) in [
            (env::GIT_TRUST_ANCHOR_KEY, "/workspace/wt/.evil-gitconfig"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.sshCommand"),
            ("GIT_CONFIG_VALUE_0", "/workspace/wt/pwn.sh"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/workspace/wt/.evil-gitconfig"),
        ] {
            let mut l = launcher();
            let mut spec = command();
            spec.environment = vec![(key.to_owned(), value.to_owned())];
            assert!(
                matches!(
                    l.create_command("hostile", invocation(), &spec),
                    Err(Error::InvalidCommand)
                ),
                "{key}={value} must be refused"
            );
            let (fs, spawn) = l.into_parts();
            assert!(fs.created.is_empty(), "no leaf may be created for {key}");
            assert!(
                spawn.exec_calls.is_empty(),
                "no child may be spawned for {key}"
            );
        }
    }

    #[test]
    fn lift_is_one_way_and_fenced() {
        let mut l = launcher();
        let mut leaf = create(&mut l, "one");
        assert!(matches!(
            l.fenced_lift(&mut leaf, 98),
            Err(Error::FenceMismatch)
        ));
        l.fenced_lift(&mut leaf, 99).unwrap();
        assert!(matches!(
            l.fenced_lift(&mut leaf, 99),
            Err(Error::LiftAlreadyApplied)
        ));
    }

    /// The whole point of the lift, asserted on the bytes written to the leaf:
    /// unleased is a throttling quota, lifted is the pod's full budget, and
    /// NEITHER is `max` — an unbounded leaf would let one build take the node.
    #[test]
    fn the_lift_writes_the_leased_quota_not_an_unbounded_max() {
        let mut l = launcher();
        let mut leaf = create(&mut l, "one");
        let fd = leaf.fd;
        assert_eq!(l.fs.files[&(fd, "cpu.max".into())], "25000 100000");
        l.fenced_lift(&mut leaf, 99).unwrap();
        let lifted = &l.fs.files[&(fd, "cpu.max".into())];
        assert_eq!(lifted, "400000 100000");
        assert!(
            !lifted.starts_with("max"),
            "an unbounded lift removes the only remaining ceiling"
        );
    }

    #[test]
    fn remove_requires_descendant_emptiness() {
        let mut l = launcher();
        let leaf = create(&mut l, "one");
        assert!(matches!(l.remove(&leaf), Err(Error::InvalidEvents)));
        let fd = leaf.fd;
        l.fs.files
            .insert((fd, "cgroup.events".into()), "populated 1\n".into());
        assert!(matches!(l.remove(&leaf), Err(Error::StillPopulated)));
        l.fs.files
            .insert((fd, "cgroup.events".into()), "populated 0\n".into());
        l.remove(&leaf).unwrap();
        assert_eq!(l.fs.removed, vec!["one"]);
    }

    #[test]
    fn readiness_rejects_all_unsupported_delegation_profiles() {
        for mode in [CgroupMode::V1, CgroupMode::Hybrid] {
            let mut fs = FakeFs::ready();
            fs.readiness.as_mut().unwrap().mode = mode;
            assert!(matches!(
                Launcher::new(fs, FakeSpawn::default(), config()),
                Err(Error::NotCgroupV2)
            ));
        }
        let mut missing_cpu = FakeFs::ready();
        missing_cpu
            .readiness
            .as_mut()
            .unwrap()
            .delegated_controllers
            .clear();
        assert!(matches!(
            Launcher::new(missing_cpu, FakeSpawn::default(), config()),
            Err(Error::OverbroadOrMissingCpuDelegation)
        ));
        let mut overbroad = FakeFs::ready();
        overbroad
            .readiness
            .as_mut()
            .unwrap()
            .delegated_controllers
            .insert("memory".into());
        assert!(matches!(
            Launcher::new(overbroad, FakeSpawn::default(), config()),
            Err(Error::OverbroadOrMissingCpuDelegation)
        ));
        let mut read_only = FakeFs::ready();
        read_only.readiness.as_mut().unwrap().root_writable = false;
        assert!(matches!(
            Launcher::new(read_only, FakeSpawn::default(), config()),
            Err(Error::ReadOnlyDelegation)
        ));
        let mut wrong_owner = FakeFs::ready();
        wrong_owner.readiness.as_mut().unwrap().owner_uid = 8;
        assert!(matches!(
            Launcher::new(wrong_owner, FakeSpawn::default(), config()),
            Err(Error::IncompatibleOwnership {
                expected: 7,
                actual: 8
            })
        ));
    }

    #[test]
    fn leaf_paths_and_actual_symlinks_are_rejected() {
        assert!(matches!(
            validate_leaf_name("../escape"),
            Err(Error::UnsafeLeafName)
        ));
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap())
            .join(format!(
                "djinn-cgroup-launcher-symlink-{}",
                std::process::id()
            ));
        let link = base.join("root-link");
        std::fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(&base, &link).unwrap();
        let result = open_dir(link.as_os_str());
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir(&base).unwrap();
        assert!(matches!(result, Err(Error::Io(_))));
    }

    #[test]
    fn malformed_command_is_rejected_before_leaf_or_spawn() {
        let mut l = launcher();
        let mut malformed = command();
        malformed.program = "/bin/../sh".into();
        assert!(matches!(
            l.create_command("one", invocation(), &malformed),
            Err(Error::InvalidCommand)
        ));
        let (fs, spawn) = l.into_parts();
        assert!(fs.created.is_empty());
        assert!(spawn.attempts.is_empty());
        assert!(spawn.exec_calls.is_empty());
    }

    #[test]
    fn command_validation_rejects_environment_keys_unrepresentable_by_execve() {
        for key in ["LC_\0X", "DJINN_KEY=override"] {
            let mut malformed = command();
            malformed.environment = vec![(key.into(), "value".into())];
            assert!(matches!(malformed.validate(), Err(Error::InvalidCommand)));
        }
    }

    /// The allow-list stays closed: widening it for the build toolchain must
    /// not have opened it to anything else.
    #[test]
    fn command_validation_still_refuses_unlisted_environment_keys() {
        for key in ["LD_PRELOAD", "ANTHROPIC_API_KEY", "GITHUB_TOKEN"] {
            let mut malformed = command();
            malformed.environment = vec![(key.into(), "value".into())];
            assert!(
                matches!(malformed.validate(), Err(Error::InvalidCommand)),
                "{key} must not pass validation"
            );
        }
        let mut build = command();
        build.environment = vec![("CARGO_TARGET_DIR".into(), "/cache/target".into())];
        assert!(build.validate().is_ok());
    }

    #[test]
    fn denied_spawn_never_executes_a_child() {
        let mut l = Launcher::new(
            FakeFs::ready(),
            FakeSpawn {
                deny: true,
                ..FakeSpawn::default()
            },
            config(),
        )
        .unwrap();
        assert!(matches!(
            l.create_command("one", invocation(), &command())
                .map(|v| v.0),
            Err(Error::SpawnDenied)
        ));
        let (fs, spawn) = l.into_parts();
        assert_eq!(spawn.attempts.len(), 1);
        assert!(spawn.exec_calls.is_empty());
        assert_eq!(fs.removed, vec!["one"]);
    }
}
