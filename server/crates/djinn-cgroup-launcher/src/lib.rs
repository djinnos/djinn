//! Fail-closed, cgroup-v2 leaf lifecycle primitives.
//!
//! This crate intentionally has no lease transport or process protocol.  The
//! caller supplies an invocation-local fence and an injectable clone syscall;
//! this boundary only creates and controls one direct child of a validated
//! delegated cgroup root.

use std::{collections::BTreeSet, ffi::CString, fs, io, os::fd::RawFd, path::Path};

use thiserror::Error;

pub mod broker;
pub mod child;
pub mod transport;

const DEFAULT_PERIOD_US: u64 = 100_000;

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
        // cpu.max quota is in microseconds per period.  1000m is one CPU.
        format!(
            "{} {DEFAULT_PERIOD_US}",
            u64::from(self.0) * DEFAULT_PERIOD_US / 1000
        )
    }
}

impl Default for UnleasedQuota {
    fn default() -> Self {
        Self(Self::DEFAULT_MILLICORES)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherConfig {
    pub unleased_quota: UnleasedQuota,
    pub expected_uid: u32,
}

impl LauncherConfig {
    pub fn new(unleased_millicores: Option<u16>, expected_uid: u32) -> Result<Self, Error> {
        Ok(Self {
            unleased_quota: UnleasedQuota::new(
                unleased_millicores.unwrap_or(UnleasedQuota::DEFAULT_MILLICORES),
            )?,
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
    pub const MAX_BYTES: usize = 16 * 1024;

    pub fn validate(&self) -> Result<(), Error> {
        if !safe_command_path(&self.program, false)
            || !safe_command_path(&self.cwd, true)
            || self.argv.len() > Self::MAX_ARGUMENTS
            || self.environment.len() > 32
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
            if !allowed_environment(key) || key.contains(['\0', '=']) || value.contains('\0') {
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
fn allowed_environment(key: &str) -> bool {
    matches!(key, "HOME" | "PATH" | "TERM" | "LANG" | "TZ" | "CI")
        || key.starts_with("LC_")
        || key.starts_with("DJINN_")
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

pub trait CloneIntoCgroup {
    /// This is the only child-spawn seam. Implementations must use
    /// `clone3(CLONE_INTO_CGROUP)` with `target_cgroup_fd` before exec.
    fn clone_into_cgroup(
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

impl<F: CgroupFs, S: CloneIntoCgroup> Launcher<F, S> {
    pub fn new(fs: F, syscall: S, config: LauncherConfig) -> Result<Self, Error> {
        fs.readiness()?.validate(config.expected_uid)?;
        Ok(Self {
            fs,
            syscall,
            config,
        })
    }

    pub fn create_command(
        &mut self,
        name: &str,
        invocation: Invocation,
        command: &CommandSpec,
    ) -> Result<(Leaf, ChildProcess), Error> {
        validate_leaf_name(name)?;
        command.validate()?;
        let fd = self.fs.create_direct_child(name)?;
        self.fs
            .write_leaf(fd, "cpu.max", &self.config.unleased_quota.cpu_max())?;
        // Clone is deliberately last: all readiness and leaf setup failures
        // occur before the child can execute.
        let child = match self.syscall.clone_into_cgroup(fd, &invocation, command) {
            Ok(child) => child,
            Err(error) => {
                // clone3 failed before a child existed, so this direct child is
                // necessarily empty. Do not leak a delegated cgroup on refusal.
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
            .write_leaf(leaf.fd, "cpu.max", &format!("max {DEFAULT_PERIOD_US}"))?;
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
    #[error("cgroup v2 is required (v1 and hybrid mounts are unsupported)")]
    NotCgroupV2,
    #[error("delegated cgroup root is read-only")]
    ReadOnlyDelegation,
    #[error("delegated cgroup owner {actual} differs from launcher uid {expected}")]
    IncompatibleOwnership { expected: u32, actual: u32 },
    #[error("delegated root must enable exactly the cpu controller")]
    OverbroadOrMissingCpuDelegation,
    #[error("leaf name is not a safe direct child")]
    UnsafeLeafName,
    #[error("clone3(CLONE_INTO_CGROUP) was denied or unsupported")]
    CloneDenied,
    #[error("command request is malformed, over-budget, or outside the broker allow-list")]
    InvalidCommand,
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

/// The production broker supplies a real clone3 implementation. This safe
/// default makes unsupported/denied kernels fail before any child exec.
pub struct DenyClone3;
impl CloneIntoCgroup for DenyClone3 {
    fn clone_into_cgroup(
        &mut self,
        _: RawFd,
        _: &Invocation,
        _: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        Err(Error::CloneDenied)
    }
}

/// Linux production clone/exec implementation. The child is born in the
/// target cgroup and crosses the credential/isolation boundary before exec.
pub struct NativeClone3;

/// Close every non-stdio descriptor with one kernel operation. In particular,
/// do not enumerate `/proc/self/fd`: `read_dir` owns a descriptor which is
/// reported by that directory and then closed when the iterator is dropped,
/// leaving a stale fd for `prepare_child` to fail on.
fn close_inherited_descriptors(
    close_range: impl FnOnce(u32, u32) -> Result<(), Error>,
) -> Result<(), Error> {
    close_range(3, u32::MAX)
}

fn native_close_inherited_descriptors(first: u32, last: u32) -> Result<(), Error> {
    let result = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0_u32) };
    if result == 0 {
        Ok(())
    } else {
        Err(Error::Io(io::Error::last_os_error()))
    }
}

impl CloneIntoCgroup for NativeClone3 {
    fn clone_into_cgroup(
        &mut self,
        cgroup: RawFd,
        _: &Invocation,
        command: &CommandSpec,
    ) -> Result<ChildProcess, Error> {
        command.validate()?;
        let mut out = [0; 2];
        let mut err = [0; 2];
        // Child writes remain blocking; pipe capacity provides backpressure.
        if unsafe { libc::pipe2(out.as_mut_ptr(), libc::O_CLOEXEC) } != 0
            || unsafe { libc::pipe2(err.as_mut_ptr(), libc::O_CLOEXEC) } != 0
        {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        #[repr(C)]
        struct Args {
            flags: u64,
            pidfd: u64,
            child_tid: u64,
            parent_tid: u64,
            exit_signal: u64,
            stack: u64,
            stack_size: u64,
            tls: u64,
            set_tid: u64,
            set_tid_size: u64,
            cgroup: u64,
        }
        let args = Args {
            flags: libc::CLONE_INTO_CGROUP as u64,
            pidfd: 0,
            child_tid: 0,
            parent_tid: 0,
            exit_signal: libc::SIGCHLD as u64,
            stack: 0,
            stack_size: 0,
            tls: 0,
            set_tid: 0,
            set_tid_size: 0,
            cgroup: cgroup as u64,
        };
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone3,
                &raw const args,
                std::mem::size_of::<Args>(),
            )
        } as i32;
        if pid < 0 {
            unsafe {
                libc::close(out[0]);
                libc::close(out[1]);
                libc::close(err[0]);
                libc::close(err[1]);
            }
            return Err(Error::CloneDenied);
        }
        if pid == 0 {
            unsafe {
                libc::close(out[0]);
                libc::close(err[0]);
                let null = CString::new("/dev/null").unwrap();
                let stdin = libc::open(null.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                if stdin < 0 || libc::dup2(stdin, 0) < 0 {
                    libc::_exit(127);
                }
                libc::close(stdin);
                libc::dup2(out[1], 1);
                libc::dup2(err[1], 2);
                libc::close(out[1]);
                libc::close(err[1]);
            }
            let mut child = child::NativeChildSyscalls;
            // A close-range operation is race-safe and leaves no procfs
            // directory iterator descriptor that could become stale before
            // the credential boundary. Stdio remains the only child surface.
            if close_inherited_descriptors(native_close_inherited_descriptors)
                .and_then(|_| {
                    child::prepare_child(&mut child, &[], &child::ChildMounts::isolated())
                })
                .is_err()
            {
                unsafe { libc::_exit(127) };
            }
            let cwd = CString::new(command.cwd.as_str()).unwrap();
            if unsafe { libc::chdir(cwd.as_ptr()) } != 0 {
                unsafe { libc::_exit(127) };
            }
            let program = CString::new(command.program.as_str()).unwrap();
            let args: Vec<CString> = command
                .argv
                .iter()
                .map(|v| CString::new(v.as_str()).unwrap())
                .collect();
            let mut argv = Vec::with_capacity(args.len() + 2);
            argv.push(program.as_ptr().cast_mut());
            argv.extend(args.iter().map(|v| v.as_ptr().cast_mut()));
            argv.push(std::ptr::null_mut());
            // Pass exactly the validated environment, never the broker's.
            let environment: Vec<CString> = command
                .environment
                .iter()
                .map(|(key, value)| CString::new(format!("{key}={value}")).unwrap())
                .collect();
            let mut envp: Vec<*mut libc::c_char> = environment
                .iter()
                .map(|value| value.as_ptr().cast_mut())
                .collect();
            envp.push(std::ptr::null_mut());
            unsafe {
                libc::execve(program.as_ptr(), argv.as_ptr().cast(), envp.as_ptr().cast());
                libc::_exit(127)
            }
        }
        unsafe {
            libc::close(out[1]);
            libc::close(err[1]);
            for fd in [out[0], err[0]] {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                    libc::close(out[0]);
                    libc::close(err[0]);
                    return Err(Error::Io(io::Error::last_os_error()));
                }
            }
        }
        Ok(ChildProcess {
            pid,
            stdout: out[0],
            stderr: err[0],
        })
    }
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
    struct FakeClone {
        attempts: Vec<(RawFd, Invocation)>,
        exec_calls: Vec<(RawFd, Invocation)>,
        deny: bool,
    }
    impl CloneIntoCgroup for FakeClone {
        fn clone_into_cgroup(
            &mut self,
            fd: RawFd,
            inv: &Invocation,
            _: &CommandSpec,
        ) -> Result<ChildProcess, Error> {
            self.attempts.push((fd, inv.clone()));
            if self.deny {
                return Err(Error::CloneDenied);
            }
            self.exec_calls.push((fd, inv.clone()));
            Ok(ChildProcess {
                pid: 1,
                stdout: -1,
                stderr: -1,
            })
        }
    }
    fn launcher() -> Launcher<FakeFs, FakeClone> {
        Launcher::new(
            FakeFs::ready(),
            FakeClone::default(),
            LauncherConfig::new(None, 7).unwrap(),
        )
        .unwrap()
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
    fn create(l: &mut Launcher<FakeFs, FakeClone>, name: &str) -> Leaf {
        l.create_command(name, invocation(), &command()).unwrap().0
    }

    #[test]
    fn quota_configuration_has_a_safe_default_and_closed_bounds() {
        assert_eq!(UnleasedQuota::default().millicores(), 250);
        assert_eq!(
            LauncherConfig::new(None, 7)
                .unwrap()
                .unleased_quota
                .millicores(),
            250
        );
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
    }

    #[test]
    fn fresh_invocation_is_passed_with_leaf_fd() {
        let mut l = launcher();
        let leaf = create(&mut l, "invocation-1");
        let (_, clone) = l.into_parts();
        assert_eq!(clone.exec_calls, vec![(leaf.fd, invocation())]);
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
                Launcher::new(
                    fs,
                    FakeClone::default(),
                    LauncherConfig::new(None, 7).unwrap()
                ),
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
            Launcher::new(
                missing_cpu,
                FakeClone::default(),
                LauncherConfig::new(None, 7).unwrap()
            ),
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
            Launcher::new(
                overbroad,
                FakeClone::default(),
                LauncherConfig::new(None, 7).unwrap()
            ),
            Err(Error::OverbroadOrMissingCpuDelegation)
        ));
        let mut read_only = FakeFs::ready();
        read_only.readiness.as_mut().unwrap().root_writable = false;
        assert!(matches!(
            Launcher::new(
                read_only,
                FakeClone::default(),
                LauncherConfig::new(None, 7).unwrap()
            ),
            Err(Error::ReadOnlyDelegation)
        ));
        let mut wrong_owner = FakeFs::ready();
        wrong_owner.readiness.as_mut().unwrap().owner_uid = 8;
        assert!(matches!(
            Launcher::new(
                wrong_owner,
                FakeClone::default(),
                LauncherConfig::new(None, 7).unwrap()
            ),
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
    fn malformed_command_is_rejected_before_leaf_or_clone() {
        let mut l = launcher();
        let mut malformed = command();
        malformed.program = "/bin/../sh".into();
        assert!(matches!(
            l.create_command("one", invocation(), &malformed),
            Err(Error::InvalidCommand)
        ));
        let (fs, clone) = l.into_parts();
        assert!(fs.created.is_empty());
        assert!(clone.attempts.is_empty());
        assert!(clone.exec_calls.is_empty());
    }

    #[test]
    fn command_validation_rejects_environment_keys_unrepresentable_by_execve() {
        for key in ["LC_\0X", "DJINN_KEY=override"] {
            let mut malformed = command();
            malformed.environment = vec![(key.into(), "value".into())];
            assert!(matches!(malformed.validate(), Err(Error::InvalidCommand)));
        }
    }

    #[derive(Default)]
    struct CloseRangePreparedChild {
        calls: Vec<&'static str>,
    }

    impl child::ChildSyscalls for CloseRangePreparedChild {
        fn close(&mut self, _: RawFd) -> Result<(), Error> {
            Err(Error::ChildPreparation("stale descriptor"))
        }
        fn set_groups_empty(&mut self) -> Result<(), Error> {
            self.calls.push("groups");
            Ok(())
        }
        fn clear_capabilities(&mut self) -> Result<(), Error> {
            self.calls.push("caps");
            Ok(())
        }
        fn set_gid(&mut self, _: u32) -> Result<(), Error> {
            self.calls.push("gid");
            Ok(())
        }
        fn set_uid(&mut self, _: u32) -> Result<(), Error> {
            self.calls.push("uid");
            Ok(())
        }
        fn set_umask(&mut self, _: u32) -> Result<(), Error> {
            self.calls.push("umask");
            Ok(())
        }
        fn set_no_new_privs(&mut self) -> Result<(), Error> {
            self.calls.push("nnp");
            Ok(())
        }
        fn install_restricted_seccomp(&mut self) -> Result<(), Error> {
            self.calls.push("seccomp");
            Ok(())
        }
    }

    #[test]
    fn close_range_seam_prepares_child_without_a_stale_procfs_descriptor() {
        let mut bounds = None;
        let mut child = CloseRangePreparedChild::default();
        close_inherited_descriptors(|first, last| {
            bounds = Some((first, last));
            Ok(())
        })
        .and_then(|_| child::prepare_child(&mut child, &[], &child::ChildMounts::isolated()))
        .unwrap();

        assert_eq!(bounds, Some((3, u32::MAX)));
        assert_eq!(
            child.calls,
            ["groups", "gid", "uid", "caps", "umask", "nnp", "seccomp"]
        );
    }

    #[test]
    fn denied_clone_never_executes_a_child() {
        let mut l = Launcher::new(
            FakeFs::ready(),
            FakeClone {
                deny: true,
                ..FakeClone::default()
            },
            LauncherConfig::new(None, 7).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            l.create_command("one", invocation(), &command())
                .map(|v| v.0),
            Err(Error::CloneDenied)
        ));
        let (fs, clone) = l.into_parts();
        assert_eq!(clone.attempts.len(), 1);
        assert!(clone.exec_calls.is_empty());
        assert_eq!(fs.removed, vec!["one"]);
    }
}
