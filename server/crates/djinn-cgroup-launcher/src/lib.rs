//! Fail-closed, cgroup-v2 leaf lifecycle primitives.
//!
//! This crate intentionally has no lease transport or process protocol.  The
//! caller supplies an invocation-local fence and an injectable clone syscall;
//! this boundary only creates and controls one direct child of a validated
//! delegated cgroup root.

use std::{collections::BTreeSet, ffi::CString, fs, io, os::fd::RawFd, path::Path};

use thiserror::Error;

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

    pub fn millicores(self) -> u16 { self.0 }

    fn cpu_max(self) -> String {
        // cpu.max quota is in microseconds per period.  1000m is one CPU.
        format!("{} {DEFAULT_PERIOD_US}", u64::from(self.0) * DEFAULT_PERIOD_US / 1000)
    }
}

impl Default for UnleasedQuota {
    fn default() -> Self { Self(Self::DEFAULT_MILLICORES) }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherConfig {
    pub unleased_quota: UnleasedQuota,
    pub expected_uid: u32,
}

impl LauncherConfig {
    pub fn new(unleased_millicores: Option<u16>, expected_uid: u32) -> Result<Self, Error> {
        Ok(Self { unleased_quota: UnleasedQuota::new(unleased_millicores.unwrap_or(UnleasedQuota::DEFAULT_MILLICORES))?, expected_uid })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CgroupMode { V1, V2, Hybrid }

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
        if self.mode != CgroupMode::V2 { return Err(Error::NotCgroupV2); }
        if !self.root_writable { return Err(Error::ReadOnlyDelegation); }
        if self.owner_uid != expected_uid { return Err(Error::IncompatibleOwnership { expected: expected_uid, actual: self.owner_uid }); }
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Leaf {
    name: String,
    fd: RawFd,
    invocation: Invocation,
    lifted: bool,
    terminal: bool,
}

impl Leaf {
    pub fn name(&self) -> &str { &self.name }
    pub fn cgroup_fd(&self) -> RawFd { self.fd }
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
    fn clone_into_cgroup(&mut self, target_cgroup_fd: RawFd, invocation: &Invocation) -> Result<(), Error>;
}

pub struct Launcher<F, S> {
    fs: F,
    syscall: S,
    config: LauncherConfig,
}

impl<F: CgroupFs, S: CloneIntoCgroup> Launcher<F, S> {
    pub fn new(fs: F, syscall: S, config: LauncherConfig) -> Result<Self, Error> {
        fs.readiness()?.validate(config.expected_uid)?;
        Ok(Self { fs, syscall, config })
    }

    pub fn create(&mut self, name: &str, invocation: Invocation) -> Result<Leaf, Error> {
        validate_leaf_name(name)?;
        let fd = self.fs.create_direct_child(name)?;
        self.fs.write_leaf(fd, "cpu.max", &self.config.unleased_quota.cpu_max())?;
        // Clone is deliberately last: all readiness and leaf setup failures
        // occur before the child can execute.
        if let Err(error) = self.syscall.clone_into_cgroup(fd, &invocation) {
            // clone3 failed before a child existed, so this direct child is
            // necessarily empty. Do not leak a delegated cgroup on refusal.
            let _ = self.fs.remove_leaf(fd, name);
            return Err(error);
        }
        Ok(Leaf { name: name.to_owned(), fd, invocation, lifted: false, terminal: false })
    }

    pub fn sample(&mut self, leaf: &Leaf) -> Result<CpuStat, Error> {
        parse_cpu_stat(&self.fs.read_leaf(leaf.fd, "cpu.stat")?)
    }

    pub fn fenced_lift(&mut self, leaf: &mut Leaf, fence: u64) -> Result<(), Error> {
        if leaf.terminal { return Err(Error::TerminalIntent); }
        if leaf.lifted { return Err(Error::LiftAlreadyApplied); }
        if leaf.invocation.fence != fence { return Err(Error::FenceMismatch); }
        self.fs.write_leaf(leaf.fd, "cpu.max", &format!("max {DEFAULT_PERIOD_US}"))?;
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
        if populated_zero(&events)? { Ok(()) } else { Err(Error::StillPopulated) }
    }

    pub fn remove(&mut self, leaf: &Leaf) -> Result<(), Error> {
        self.wait_empty(leaf)?;
        self.fs.remove_leaf(leaf.fd, &leaf.name)
    }

    pub fn into_parts(self) -> (F, S) { (self.fs, self.syscall) }
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
        let Some(raw) = fields.next() else { return Err(Error::InvalidCpuStat); };
        if fields.next().is_some() { return Err(Error::InvalidCpuStat); }
        let value = raw.parse().map_err(|_| Error::InvalidCpuStat)?;
        match key {
            "usage_usec" => result.usage_usec = value,
            "user_usec" => result.user_usec = value,
            "system_usec" => result.system_usec = value,
            "nr_periods" => result.nr_periods = value,
            "nr_throttled" => result.nr_throttled = value,
            "throttled_usec" => result.throttled_usec = value,
            _ => {},
        }
    }
    Ok(result)
}

fn populated_zero(input: &str) -> Result<bool, Error> {
    for line in input.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() == Some("populated") {
            return match (fields.next(), fields.next()) { (Some("0"), None) => Ok(true), (Some("1"), None) => Ok(false), _ => Err(Error::InvalidEvents) };
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
    #[error("filesystem operation failed: {0}")]
    Io(#[from] io::Error),
}

/// Linux no-follow implementation for the filesystem seam. It only ever uses
/// `openat` relative to its already opened delegated root or leaf descriptor.
pub struct NativeCgroupFs { root: RawFd, readiness: Readiness }

impl NativeCgroupFs {
    pub fn open(root: impl AsRef<Path>, expected_uid: u32) -> Result<Self, Error> {
        let root_path = root.as_ref().to_path_buf();
        let metadata = fs::symlink_metadata(&root_path)?;
        if metadata.file_type().is_symlink() { return Err(Error::UnsafeLeafName); }
        #[cfg(unix)]
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        #[cfg(unix)]
        let owner_uid = metadata.uid();
        #[cfg(unix)]
        let root_writable = metadata.permissions().mode() & 0o222 != 0 && metadata.permissions().mode() & 0o022 == 0;
        let controllers = fs::read_to_string(root_path.join("cgroup.subtree_control"))?
            .split_ascii_whitespace().map(str::to_owned).collect();
        let mode = inspect_cgroup_mode()?;
        let readiness = Readiness { mode, root_writable, owner_uid, delegated_controllers: controllers };
        readiness.validate(expected_uid)?;
        let root = open_dir(root_path.as_os_str())?;
        Ok(Self { root, readiness })
    }
}

impl Drop for NativeCgroupFs { fn drop(&mut self) { unsafe { libc::close(self.root); } } }

impl CgroupFs for NativeCgroupFs {
    fn readiness(&self) -> Result<Readiness, Error> { Ok(self.readiness.clone()) }
    fn create_direct_child(&mut self, name: &str) -> Result<RawFd, Error> {
        validate_leaf_name(name)?;
        let name = CString::new(name).map_err(|_| Error::UnsafeLeafName)?;
        let rc = unsafe { libc::mkdirat(self.root, name.as_ptr(), 0o700) };
        if rc != 0 { return Err(io::Error::last_os_error().into()); }
        let fd = unsafe { libc::openat(self.root, name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 { return Err(io::Error::last_os_error().into()); }
        Ok(fd)
    }
    fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), Error> {
        let file = safe_control_file(file)?;
        let target = unsafe { libc::openat(fd, file.as_ptr(), libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if target < 0 { return Err(io::Error::last_os_error().into()); }
        let bytes = value.as_bytes();
        let written = unsafe { libc::write(target, bytes.as_ptr().cast(), bytes.len()) };
        unsafe { libc::close(target); }
        if written != bytes.len() as isize { return Err(io::Error::last_os_error().into()); }
        Ok(())
    }
    fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, Error> {
        let file = safe_control_file(file)?;
        let target = unsafe { libc::openat(fd, file.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if target < 0 { return Err(io::Error::last_os_error().into()); }
        let mut bytes = Vec::new(); let mut chunk = [0_u8; 4096];
        loop { let count = unsafe { libc::read(target, chunk.as_mut_ptr().cast(), chunk.len()) }; if count < 0 { unsafe { libc::close(target); } return Err(io::Error::last_os_error().into()); } if count == 0 { break; } bytes.extend_from_slice(&chunk[..count as usize]); }
        unsafe { libc::close(target); }
        String::from_utf8(bytes).map_err(|_| Error::InvalidCpuStat)
    }
    fn remove_leaf(&mut self, fd: RawFd, name: &str) -> Result<(), Error> {
        unsafe { libc::close(fd); }
        let name = CString::new(name).map_err(|_| Error::UnsafeLeafName)?;
        if unsafe { libc::unlinkat(self.root, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 { return Err(io::Error::last_os_error().into()); }
        Ok(())
    }
}

fn safe_control_file(file: &str) -> Result<CString, Error> {
    if file.contains('/') || file.contains('\0') { return Err(Error::UnsafeLeafName); }
    CString::new(file).map_err(|_| Error::UnsafeLeafName)
}
fn open_dir(path: &std::ffi::OsStr) -> Result<RawFd, Error> {
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_bytes()).map_err(|_| Error::UnsafeLeafName)?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 { Err(io::Error::last_os_error().into()) } else { Ok(fd) }
}
fn inspect_cgroup_mode() -> Result<CgroupMode, Error> {
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    let has_v1 = mounts.lines().any(|line| line.split(" - ").nth(1).is_some_and(|tail| tail.starts_with("cgroup ")));
    let has_v2 = mounts.lines().any(|line| line.split(" - ").nth(1).is_some_and(|tail| tail.starts_with("cgroup2 ")));
    Ok(match (has_v1, has_v2) { (false, true) => CgroupMode::V2, (true, true) => CgroupMode::Hybrid, _ => CgroupMode::V1 })
}

/// The production broker supplies a real clone3 implementation. This safe
/// default makes unsupported/denied kernels fail before any child exec.
pub struct DenyClone3;
impl CloneIntoCgroup for DenyClone3 { fn clone_into_cgroup(&mut self, _: RawFd, _: &Invocation) -> Result<(), Error> { Err(Error::CloneDenied) } }

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)] struct FakeFs { readiness: Option<Readiness>, files: HashMap<(RawFd, String), String>, created: Vec<String>, removed: Vec<String>, next: i32 }
    impl FakeFs { fn ready() -> Self { Self { readiness: Some(Readiness { mode: CgroupMode::V2, root_writable: true, owner_uid: 7, delegated_controllers: BTreeSet::from(["cpu".into()]) }), next: 10, ..Self::default() } } }
    impl CgroupFs for FakeFs {
        fn readiness(&self) -> Result<Readiness, Error> { Ok(self.readiness.clone().unwrap()) }
        fn create_direct_child(&mut self, name: &str) -> Result<RawFd, Error> { self.created.push(name.into()); self.next += 1; Ok(self.next) }
        fn write_leaf(&mut self, fd: RawFd, file: &str, value: &str) -> Result<(), Error> { self.files.insert((fd, file.into()), value.into()); Ok(()) }
        fn read_leaf(&mut self, fd: RawFd, file: &str) -> Result<String, Error> { Ok(self.files.get(&(fd, file.into())).cloned().unwrap_or_default()) }
        fn remove_leaf(&mut self, _: RawFd, name: &str) -> Result<(), Error> { self.removed.push(name.into()); Ok(()) }
    }
    #[derive(Default)] struct FakeClone { calls: Vec<(RawFd, Invocation)>, deny: bool }
    impl CloneIntoCgroup for FakeClone { fn clone_into_cgroup(&mut self, fd: RawFd, inv: &Invocation) -> Result<(), Error> { if self.deny { return Err(Error::CloneDenied); } self.calls.push((fd, inv.clone())); Ok(()) } }
    fn launcher() -> Launcher<FakeFs, FakeClone> { Launcher::new(FakeFs::ready(), FakeClone::default(), LauncherConfig::new(None, 7).unwrap()).unwrap() }
    fn invocation() -> Invocation { Invocation { id: "run-1".into(), fence: 99 } }

    #[test] fn fresh_invocation_is_passed_with_leaf_fd() { let mut l = launcher(); let leaf = l.create("invocation-1", invocation()).unwrap(); let (_, clone) = l.into_parts(); assert_eq!(clone.calls, vec![(leaf.fd, invocation())]); }
    #[test] fn lift_is_one_way_and_fenced() { let mut l = launcher(); let mut leaf = l.create("one", invocation()).unwrap(); assert!(matches!(l.fenced_lift(&mut leaf, 98), Err(Error::FenceMismatch))); l.fenced_lift(&mut leaf, 99).unwrap(); assert!(matches!(l.fenced_lift(&mut leaf, 99), Err(Error::LiftAlreadyApplied))); }
    #[test] fn remove_requires_descendant_emptiness() { let mut l = launcher(); let leaf = l.create("one", invocation()).unwrap(); assert!(matches!(l.remove(&leaf), Err(Error::InvalidEvents))); let fd = leaf.fd; l.fs.files.insert((fd, "cgroup.events".into()), "populated 1\n".into()); assert!(matches!(l.remove(&leaf), Err(Error::StillPopulated))); l.fs.files.insert((fd, "cgroup.events".into()), "populated 0\n".into()); l.remove(&leaf).unwrap(); assert_eq!(l.fs.removed, vec!["one"]); }
    #[test] fn readiness_and_paths_fail_closed() { for mode in [CgroupMode::V1, CgroupMode::Hybrid] { let mut fs = FakeFs::ready(); fs.readiness.as_mut().unwrap().mode = mode; assert!(matches!(Launcher::new(fs, FakeClone::default(), LauncherConfig::new(None, 7).unwrap()), Err(Error::NotCgroupV2))); } let mut fs = FakeFs::ready(); fs.readiness.as_mut().unwrap().delegated_controllers.insert("memory".into()); assert!(matches!(Launcher::new(fs, FakeClone::default(), LauncherConfig::new(None, 7).unwrap()), Err(Error::OverbroadOrMissingCpuDelegation))); assert!(matches!(validate_leaf_name("../escape"), Err(Error::UnsafeLeafName))); }
    #[test] fn denied_clone_never_reports_a_leaf() { let mut l = Launcher::new(FakeFs::ready(), FakeClone { deny: true, ..FakeClone::default() }, LauncherConfig::new(None, 7).unwrap()).unwrap(); assert!(matches!(l.create("one", invocation()), Err(Error::CloneDenied))); }
}
