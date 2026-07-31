//! Linux `openat`-relative implementation of the cgroup filesystem seam.
//!
//! Split out of `lib.rs` so the crate root stays inside the repository Rust
//! source-size guard (`scripts/check-file-size.sh`). This module is a verbatim
//! move: it holds the native no-follow [`CgroupFs`] implementation and the
//! cgroup-v2 mount interrogation helpers it needs.

use std::{ffi::CString, fs, io, os::fd::RawFd, path::Path};

use crate::{CgroupFs, CgroupMode, Error, Readiness, validate_leaf_name};

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
pub(crate) fn assert_cgroup2_filesystem(path: &Path) -> Result<(), Error> {
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
pub(crate) fn open_dir(path: &std::ffi::OsStr) -> Result<RawFd, Error> {
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
