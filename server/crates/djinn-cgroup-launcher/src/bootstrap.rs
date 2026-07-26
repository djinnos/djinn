//! Startup bootstrap: the launcher establishes its OWN delegated cgroup v2 root,
//! then drops the capability that let it.
//!
//! # Why the launcher mounts it rather than receiving it (task 7deu)
//!
//! A delegated cgroup v2 subtree is not something a Kubernetes *volume source*
//! can supply. An `emptyDir` is an ordinary directory on the node filesystem
//! with no `cgroup.subtree_control` and no `cpu.max`; a `hostPath` onto a node
//! subtree is refused from inside the pod's private, `nsdelegate`d cgroup
//! namespace. Shipping either is how the sidecar came to CrashLoopBackOff on
//! every task-run Pod.
//!
//! What *does* work — and is what this module does — is the launcher mounting
//! `cgroup2` inside its own cgroup namespace, where the mount's root IS the
//! launcher's container cgroup and every invocation leaf is a descendant of it.
//! The `emptyDir` rendered at the mount path supplies only a writable
//! **mountpoint**; `readOnlyRootFilesystem: true` forbids creating one on the
//! image's own rootfs, and `no_new_privs` does not block `mount(2)` (it blocks
//! *gaining* privilege across `execve`, not using a capability already held).
//!
//! # Why the capability does not stay
//!
//! `CAP_SYS_ADMIN` is a node-wide escape primitive: with it a process can
//! `umount` the runtime's `/proc` masks and write `/proc/sys/kernel/core_pattern`,
//! which is not namespaced. On a host whose `core_pattern` is already a pipe —
//! the production VPS's is `|/usr/share/apport/apport …` — that is a direct path
//! to executing a payload as root outside every namespace.
//!
//! So the capability exists for exactly one phase. [`Bootstrap::run`] mounts,
//! delegates, and then [`drop_bootstrap_capabilities`] removes `CAP_SYS_ADMIN`
//! and `CAP_SYS_RESOURCE` from the launcher's permitted, effective, inheritable
//! **and bounding** sets — irreversibly, before the broker binds its socket and
//! therefore before the pod can accept a single unit of work. No user-controlled
//! code ever executes while the capability is held: the only thing running in
//! that window is this function.
//!
//! That drop is the ONLY layer under the capability, and it therefore has to
//! hold on its own. `hostUsers: false` was once rendered as a second layer, on
//! the reasoning that a user namespace puts the non-namespaced sysctls out of
//! reach; it was removed because it does not work. Kubernetes user namespaces do
//! not delegate the container's cgroup to the mapped user, so the launcher's own
//! root stays owned by an unmapped uid and [`Bootstrap::vacate_root`] cannot
//! create the holding leaf — the mount succeeds and the delegation then fails
//! with `EACCES`. See `djinn_k8s::launcher::pod_host_users` for the measurement.
//!
//! This is also the honest implementation of goxi's "allowPrivilegeEscalation
//! false AFTER INITIALIZATION", which is not expressible as a Pod field — a
//! container `securityContext` is static. `CAP_SETPCAP` is what makes it real.

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

use crate::Error;

/// Capability numbers (`include/uapi/linux/capability.h`).
const CAP_SETGID: u32 = 6;
const CAP_SETUID: u32 = 7;
const CAP_SETPCAP: u32 = 8;
const CAP_SYS_ADMIN: u32 = 21;
const CAP_SYS_RESOURCE: u32 = 24;

/// The only capabilities the launcher keeps once bootstrap is complete: exactly
/// what `child::prepare_child` needs to move a child across the credential
/// boundary before `execve`.
const RETAINED_CAPABILITIES: &[u32] = &[CAP_SETGID, CAP_SETUID, CAP_SETPCAP];

/// The capabilities bootstrap needs and then destroys.
const BOOTSTRAP_ONLY_CAPABILITIES: &[u32] = &[CAP_SYS_ADMIN, CAP_SYS_RESOURCE];

/// `_LINUX_CAPABILITY_VERSION_3`.
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// Name of the holding leaf the launcher moves its own process into.
///
/// cgroup v2's "no internal process" rule exempts only the true root: a cgroup
/// may not both contain processes and enable controllers for its children. The
/// launcher's own init process starts in the mount root, so it has to move out
/// before `+cpu` can be delegated.
pub const INIT_LEAF: &str = "init";

/// Controller the delegated root enables for its children — exactly one.
const DELEGATED_CONTROLLER: &str = "cpu";

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Establish the delegated cgroup v2 root at `root`.
///
/// Idempotent: a launcher restarting inside a container whose mount already
/// exists re-uses it rather than failing. Every step that cannot be made to
/// hold is a named error — there is no partial-success path.
pub struct Bootstrap {
    root: PathBuf,
}

impl Bootstrap {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Mount, vacate, delegate, then drop the bootstrap capabilities.
    ///
    /// After this returns `Ok`, the launcher holds only
    /// [`RETAINED_CAPABILITIES`] and `root` satisfies the full
    /// [`Readiness`](crate::Readiness) contract.
    pub fn run(&self) -> Result<(), Error> {
        self.mount_cgroup2()?;
        self.vacate_root()?;
        self.delegate_cpu()?;
        drop_bootstrap_capabilities()
    }

    /// Mount a private, read-write `cgroup2` filesystem at `root`.
    ///
    /// Inside the pod's private cgroup namespace the mount's root is the
    /// launcher's own container cgroup, so an invocation leaf created under it
    /// is a descendant of the namespace root — which is what makes the leaf
    /// reachable at all under `nsdelegate`.
    fn mount_cgroup2(&self) -> Result<(), Error> {
        if !self.root.is_dir() {
            std::fs::create_dir_all(&self.root).map_err(|error| Error::CgroupMountFailed {
                path: self.root.display().to_string(),
                errno: error.raw_os_error().unwrap_or(0),
            })?;
        }
        if crate::is_cgroup2_filesystem(&self.root)? {
            // Already mounted (launcher restart inside a live container).
            return Ok(());
        }

        let target = c_path(&self.root)?;
        let fstype = CString::new("cgroup2").map_err(|_| Error::UnsafeLeafName)?;
        let source = CString::new("cgroup2").map_err(|_| Error::UnsafeLeafName)?;
        // SAFETY: all three pointers are live NUL-terminated strings and `data`
        // is NULL, which `cgroup2` accepts.
        let rc = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                (libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) as libc::c_ulong,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(Error::CgroupMountFailed {
                path: self.root.display().to_string(),
                errno: errno(),
            });
        }
        // Refuse to proceed on a mount that somehow is not cgroup2 rather than
        // discovering it three control-file reads later.
        if !crate::is_cgroup2_filesystem(&self.root)? {
            return Err(Error::CgroupMountFailed {
                path: self.root.display().to_string(),
                errno: 0,
            });
        }
        Ok(())
    }

    /// Move every process sitting directly in the mount root into [`INIT_LEAF`].
    fn vacate_root(&self) -> Result<(), Error> {
        let init = self.root.join(INIT_LEAF);
        if !init.is_dir() {
            std::fs::create_dir(&init).map_err(|error| Error::CgroupDelegationFailed {
                detail: "create the init holding leaf",
                errno: error.raw_os_error().unwrap_or(0),
            })?;
        }
        let procs = std::fs::read_to_string(self.root.join("cgroup.procs")).map_err(|error| {
            Error::CgroupDelegationFailed {
                detail: "read the delegated root's cgroup.procs",
                errno: error.raw_os_error().unwrap_or(0),
            }
        })?;
        let target = init.join("cgroup.procs");
        for pid in procs.split_ascii_whitespace() {
            std::fs::write(&target, pid).map_err(|error| Error::CgroupDelegationFailed {
                detail: "move the launcher's own process into the init leaf",
                errno: error.raw_os_error().unwrap_or(0),
            })?;
        }
        Ok(())
    }

    /// Enable exactly the `cpu` controller for the root's children.
    ///
    /// Also *disables* anything else already enabled, because the launcher's
    /// readiness contract requires the delegated controller set to be exactly
    /// `{cpu}` — a broader delegation would hand the launcher authority it has
    /// no reason to hold.
    fn delegate_cpu(&self) -> Result<(), Error> {
        let control = self.root.join("cgroup.subtree_control");
        let enabled =
            std::fs::read_to_string(&control).map_err(|error| Error::CgroupDelegationFailed {
                detail: "read cgroup.subtree_control",
                errno: error.raw_os_error().unwrap_or(0),
            })?;
        let mut directives: Vec<String> = enabled
            .split_ascii_whitespace()
            .filter(|controller| *controller != DELEGATED_CONTROLLER)
            .map(|controller| format!("-{controller}"))
            .collect();
        if !enabled
            .split_ascii_whitespace()
            .any(|controller| controller == DELEGATED_CONTROLLER)
        {
            directives.push(format!("+{DELEGATED_CONTROLLER}"));
        }
        if directives.is_empty() {
            return Ok(());
        }
        std::fs::write(&control, directives.join(" ")).map_err(|error| {
            Error::CgroupDelegationFailed {
                // The overwhelmingly common cause is the PARENT (the kubelet's
                // pod cgroup, outside this namespace) not offering `cpu` to its
                // children, which shows up as EINVAL here.
                detail: "enable the cpu controller in cgroup.subtree_control",
                errno: error.raw_os_error().unwrap_or(0),
            }
        })
    }
}

/// Irreversibly remove [`BOOTSTRAP_ONLY_CAPABILITIES`], keeping only
/// [`RETAINED_CAPABILITIES`].
///
/// Drops from the bounding set first (so the capability cannot be regained via
/// a file-capability `execve`), then rewrites permitted/effective/inheritable.
/// Ordering matters: `PR_CAPBSET_DROP` itself requires `CAP_SETPCAP`, which is
/// retained, but `CAP_SYS_ADMIN` must still be in the effective set for nothing
/// — it is not needed here, so the bounding drops happen while both are present
/// only for simplicity, not necessity.
pub fn drop_bootstrap_capabilities() -> Result<(), Error> {
    for capability in BOOTSTRAP_ONLY_CAPABILITIES {
        // ENODATA/EINVAL for a capability not in the bounding set is not a
        // failure to drop it — it is already absent.
        let rc = unsafe {
            libc::prctl(
                libc::PR_CAPBSET_DROP,
                libc::c_ulong::from(*capability),
                0,
                0,
                0,
            )
        };
        if rc != 0 && errno() != libc::EINVAL {
            return Err(Error::CapabilityDropFailed { errno: errno() });
        }
    }

    let retained = capability_mask(RETAINED_CAPABILITIES);
    let header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [
        CapabilityData {
            effective: retained,
            permitted: retained,
            inheritable: 0,
        },
        CapabilityData::default(),
    ];
    // SAFETY: `capset` reads a version-3 header and two data words through live
    // pointers; both live for the duration of the call.
    if unsafe { libc::syscall(libc::SYS_capset, &raw const header, data.as_ptr()) } != 0 {
        return Err(Error::CapabilityDropFailed { errno: errno() });
    }
    // Prove it, rather than trust the return value: this is the step that
    // decides whether a task-run pod carries a node-wide escape primitive.
    if holds_any_bootstrap_capability()? {
        return Err(Error::CapabilityDropFailed { errno: 0 });
    }
    Ok(())
}

/// Does this process still hold any of [`BOOTSTRAP_ONLY_CAPABILITIES`] in its
/// permitted or effective set?
pub fn holds_any_bootstrap_capability() -> Result<bool, Error> {
    let header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapabilityData::default(); 2];
    // SAFETY: `capget` writes two data words through a live pointer.
    if unsafe { libc::syscall(libc::SYS_capget, &raw const header, data.as_mut_ptr()) } != 0 {
        return Err(Error::CapabilityDropFailed { errno: errno() });
    }
    let mask = capability_mask(BOOTSTRAP_ONLY_CAPABILITIES);
    Ok(data[0].permitted & mask != 0 || data[0].effective & mask != 0)
}

fn capability_mask(capabilities: &[u32]) -> u32 {
    capabilities
        .iter()
        .fold(0_u32, |mask, capability| mask | (1 << capability))
}

fn c_path(path: &Path) -> Result<CString, Error> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::UnsafeLeafName)
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retained_set_is_exactly_what_the_credential_boundary_needs() {
        assert_eq!(
            RETAINED_CAPABILITIES,
            &[CAP_SETGID, CAP_SETUID, CAP_SETPCAP]
        );
        assert_eq!(
            capability_mask(RETAINED_CAPABILITIES),
            (1 << 6) | (1 << 7) | (1 << 8)
        );
    }

    /// `CAP_SYS_ADMIN` is the escape primitive; it must never be retained, and
    /// the retained and bootstrap-only sets must not overlap.
    #[test]
    fn the_escape_capability_is_never_retained() {
        assert!(BOOTSTRAP_ONLY_CAPABILITIES.contains(&CAP_SYS_ADMIN));
        assert!(BOOTSTRAP_ONLY_CAPABILITIES.contains(&CAP_SYS_RESOURCE));
        assert_eq!(
            capability_mask(RETAINED_CAPABILITIES) & capability_mask(BOOTSTRAP_ONLY_CAPABILITIES),
            0,
            "a capability cannot be both dropped and retained"
        );
    }

    /// The kernel's own numbering. A wrong bit here would silently retain
    /// `CAP_SYS_ADMIN` while reporting success, which is the exact class of
    /// defect (a constant that is not the value it claims to be) that made the
    /// previous clone flag a no-op.
    #[test]
    fn capability_numbers_are_the_kernel_values() {
        assert_eq!(CAP_SETGID, 6);
        assert_eq!(CAP_SETUID, 7);
        assert_eq!(CAP_SETPCAP, 8);
        assert_eq!(CAP_SYS_ADMIN, 21);
        assert_eq!(CAP_SYS_RESOURCE, 24);
        assert_eq!(CAPABILITY_VERSION_3, 0x2008_0522);
    }

    /// An ordinary unprivileged test process holds neither bootstrap
    /// capability, so the live `capget` path is exercised on every test run.
    #[test]
    fn an_unprivileged_process_reports_no_bootstrap_capability() {
        if unsafe { libc::geteuid() } == 0 {
            // Running as root (the privileged CI lane): the assertion below
            // would be about the lane's own capability set, not about this
            // code, so only exercise the syscall path.
            holds_any_bootstrap_capability().expect("capget must succeed");
            return;
        }
        assert!(
            !holds_any_bootstrap_capability().expect("capget must succeed"),
            "an unprivileged process cannot hold CAP_SYS_ADMIN"
        );
    }

    #[test]
    fn a_non_cgroup_root_fails_the_mount_step_by_name() {
        // Unprivileged: `mount(2)` is denied, which is exactly the named error
        // an unarmed launcher must produce rather than an opaque EPERM.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("djinn-bootstrap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let error = Bootstrap::new(&base)
            .run()
            .expect_err("an unprivileged mount must fail");
        let _ = std::fs::remove_dir_all(&base);
        match error {
            Error::CgroupMountFailed { path, errno } => {
                assert_eq!(path, base.display().to_string());
                assert_eq!(errno, libc::EPERM);
            }
            other => panic!("expected a named mount failure, got: {other}"),
        }
    }
}
